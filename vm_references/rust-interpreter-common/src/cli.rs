use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use crate::machine::{
    DEFAULT_INSTRUCTION_LIMIT, DEFAULT_OUTPUT_LIMIT, Engine, LoadedProgram, MAX_INPUT_LENGTH,
    MAX_INSTRUCTION_LIMIT, MAX_OUTPUT_LIMIT,
};
use crate::memory::ADDRESS_SPACE_SIZE;
use crate::protocol::{self, MAX_PAYLOAD};

const MAX_INSPECTION_COUNT: usize = 1024;
const MAX_INSPECTION_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Default)]
struct RunArguments {
    elf: Option<PathBuf>,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    result: Option<PathBuf>,
    state: Option<PathBuf>,
    instruction_limit: Option<u64>,
    output_limit: Option<u32>,
    inspect: Vec<(u32, u32)>,
}

pub fn main<E: Engine + Default>() -> i32 {
    match command::<E>() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("rv32vm: {error}");
            2
        }
    }
}

fn command<E: Engine + Default>() -> Result<(), String> {
    let mut arguments = env::args_os().skip(1);
    match arguments.next().as_deref().and_then(|value| value.to_str()) {
        Some("run") => run::<E>(parse_run(arguments.collect())?),
        Some("serve") if arguments.next().is_none() => protocol::serve::<E>(),
        Some("serve") => Err("serve accepts no arguments".into()),
        _ => Err("expected `run` or `serve`".into()),
    }
}

fn parse_run(values: Vec<OsString>) -> Result<RunArguments, String> {
    let mut result = RunArguments::default();
    let mut values = values.into_iter();
    while let Some(option) = values.next() {
        let option = option
            .to_str()
            .ok_or_else(|| "option name is not valid UTF-8".to_string())?;
        let value = values
            .next()
            .ok_or_else(|| format!("{option} requires a value"))?;
        match option {
            "--elf" => set_path(&mut result.elf, value, option)?,
            "--input" => set_path(&mut result.input, value, option)?,
            "--output" => set_path(&mut result.output, value, option)?,
            "--result" => set_path(&mut result.result, value, option)?,
            "--state" => set_path(&mut result.state, value, option)?,
            "--instruction-limit" => {
                if result.instruction_limit.is_some() {
                    return Err(format!("{option} was repeated"));
                }
                result.instruction_limit = Some(parse_decimal(&value, option)?);
            }
            "--output-limit" => {
                if result.output_limit.is_some() {
                    return Err(format!("{option} was repeated"));
                }
                let parsed = parse_decimal(&value, option)?;
                result.output_limit =
                    Some(u32::try_from(parsed).map_err(|_| format!("invalid {option}"))?);
            }
            "--inspect" => result.inspect.push(parse_inspect(&value)?),
            _ => return Err(format!("unknown option {option}")),
        }
    }
    if result.inspect.len() > MAX_INSPECTION_COUNT {
        return Err("inspection count exceeds 1024 ranges".into());
    }
    if result
        .inspect
        .iter()
        .map(|(_, length)| u64::from(*length))
        .sum::<u64>()
        > MAX_INSPECTION_BYTES
    {
        return Err("aggregate inspection length exceeds 8388608 bytes".into());
    }
    if !result.inspect.is_empty() && result.state.is_none() {
        return Err("--inspect requires --state".into());
    }
    Ok(result)
}

fn set_path(slot: &mut Option<PathBuf>, value: OsString, option: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{option} was repeated"));
    }
    *slot = Some(value.into());
    Ok(())
}

fn parse_decimal(value: &OsString, option: &str) -> Result<u64, String> {
    let value = value.to_str().ok_or_else(|| format!("invalid {option}"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid {option}"));
    }
    value.parse().map_err(|_| format!("invalid {option}"))
}

fn parse_inspect(value: &OsString) -> Result<(u32, u32), String> {
    let value = value
        .to_str()
        .ok_or_else(|| "inspect range must be ADDR:LENGTH".to_string())?;
    let (address, length) = value
        .split_once(':')
        .ok_or_else(|| "inspect range must be ADDR:LENGTH".to_string())?;
    let address = parse_address(address)?;
    let length = parse_address(length)?;
    if address > ADDRESS_SPACE_SIZE
        || u64::from(address) + u64::from(length) > u64::from(ADDRESS_SPACE_SIZE)
    {
        return Err("inspect range is outside guest address space".into());
    }
    Ok((address, length))
}

fn parse_address(value: &str) -> Result<u32, String> {
    if value == "0" {
        return Ok(0);
    }
    let parsed = if let Some(digits) = value.strip_prefix("0x") {
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("inspect range must be ADDR:LENGTH".into());
        }
        u64::from_str_radix(digits, 16)
    } else {
        if value.starts_with('0')
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() && byte.is_ascii())
        {
            return Err("inspect range must be ADDR:LENGTH".into());
        }
        value.parse()
    }
    .map_err(|_| "inspect range must be ADDR:LENGTH".to_string())?;
    u32::try_from(parsed).map_err(|_| "inspect range is outside guest address space".into())
}

fn required_path(value: Option<PathBuf>, option: &str) -> Result<PathBuf, String> {
    value.ok_or_else(|| format!("{option} is required"))
}

fn run<E: Engine + Default>(arguments: RunArguments) -> Result<(), String> {
    let instruction_limit = arguments
        .instruction_limit
        .unwrap_or(DEFAULT_INSTRUCTION_LIMIT);
    if instruction_limit > MAX_INSTRUCTION_LIMIT {
        return Err(format!(
            "instruction limit must be in the range 0..{MAX_INSTRUCTION_LIMIT}"
        ));
    }
    let output_limit = arguments.output_limit.unwrap_or(DEFAULT_OUTPUT_LIMIT);
    if output_limit > MAX_OUTPUT_LIMIT {
        return Err(format!(
            "output limit must be in the range 0..{MAX_OUTPUT_LIMIT}"
        ));
    }

    let elf_path = required_path(arguments.elf, "--elf")?;
    let input_path = required_path(arguments.input, "--input")?;
    let output_path = required_path(arguments.output, "--output")?;
    let result_path = required_path(arguments.result, "--result")?;

    let elf = fs::read(elf_path).map_err(|error| error.to_string())?;
    if elf.len() > MAX_PAYLOAD {
        return Err("ELF exceeds the 8388608-byte protocol limit".into());
    }
    let input = fs::read(input_path).map_err(|error| error.to_string())?;
    if input.len() > MAX_INPUT_LENGTH {
        return Err("input exceeds 4194304 bytes".into());
    }

    let mut program = LoadedProgram::<E>::new(&elf)?;
    let completed = program.run(&input, instruction_limit, output_limit);
    let result = completed.result.json();
    let machine = completed.machine;

    let state = if arguments.state.is_some() {
        Some(state_json(&machine, &arguments.inspect)?)
    } else {
        None
    };
    fs::write(output_path, &machine.output).map_err(|error| error.to_string())?;
    fs::write(result_path, result).map_err(|error| error.to_string())?;
    if let (Some(path), Some(state)) = (arguments.state, state) {
        fs::write(path, state).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn state_json(machine: &crate::machine::Machine, ranges: &[(u32, u32)]) -> Result<String, String> {
    let mut result = format!(
        "{{\"schema_version\":1,\"pc\":{},\"registers\":[",
        machine.pc
    );
    for (index, register) in machine.registers.iter().enumerate() {
        if index != 0 {
            result.push(',');
        }
        write!(result, "{register}").unwrap();
    }
    result.push_str("],\"memory\":[");
    for (index, (address, length)) in ranges.iter().enumerate() {
        if index != 0 {
            result.push(',');
        }
        let data = machine.memory.inspect(*address, *length)?;
        write!(
            result,
            "{{\"address\":{address},\"data_base64\":\"{}\"}}",
            base64(&data)
        )
        .unwrap();
    }
    write!(
        result,
        "],\"retired_instructions\":{},\"output_length\":{}}}",
        machine.retired,
        machine.output.len()
    )
    .unwrap();
    Ok(result)
}

fn base64(data: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        result.push(DIGITS[((value >> 18) & 63) as usize] as char);
        result.push(DIGITS[((value >> 12) & 63) as usize] as char);
        result.push(
            chunk
                .get(1)
                .map_or('=', |_| DIGITS[((value >> 6) & 63) as usize] as char),
        );
        result.push(
            chunk
                .get(2)
                .map_or('=', |_| DIGITS[(value & 63) as usize] as char),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{base64, parse_address};

    #[test]
    fn parses_canonical_addresses() {
        assert_eq!(parse_address("0").unwrap(), 0);
        assert_eq!(parse_address("31").unwrap(), 31);
        assert_eq!(parse_address("0x1f").unwrap(), 31);
        assert!(parse_address("01").is_err());
        assert!(parse_address("-1").is_err());
        assert!(parse_address("0X1f").is_err());
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
