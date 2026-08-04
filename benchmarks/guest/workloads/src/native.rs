//! In-process timing support for host-native workload executables.

use std::env;
use std::hint::black_box;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::Instant;

use crate::Workload;

fn count(value: Option<String>, name: &str, allow_zero: bool) -> Result<usize, String> {
    let value = value.ok_or_else(|| format!("missing {name}"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{name} must be an integer"));
    }
    let count = value
        .parse::<usize>()
        .map_err(|_| format!("{name} is too large"))?;
    if !allow_zero && count == 0 {
        return Err(format!("{name} must be positive"));
    }
    Ok(count)
}

fn execute(workload: Workload, input: &[u8]) -> [u8; 12] {
    black_box(black_box(workload)(black_box(input)))
}

fn command(workload: Workload) -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let warmups = count(arguments.next(), "warmups", true)?;
    let repetitions = count(arguments.next(), "repetitions", false)?;
    if arguments.next().is_some() {
        return Err("expected WARMUPS REPETITIONS".into());
    }

    let mut input = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut input)
        .map_err(|error| format!("failed to read input: {error}"))?;

    let expected = execute(workload, &input);
    for _ in 0..warmups {
        if execute(workload, &input) != expected {
            return Err("workload output changed during warmup".into());
        }
    }

    let mut samples = Vec::with_capacity(repetitions);
    for _ in 0..repetitions {
        let started = Instant::now();
        let output = execute(workload, &input);
        let elapsed = started.elapsed().as_nanos().max(1);
        if output != expected {
            return Err("workload output changed during measurement".into());
        }
        samples.push(elapsed);
    }

    let output_hex = expected
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let samples_json = samples
        .iter()
        .map(u128::to_string)
        .collect::<Vec<_>>()
        .join(",");
    writeln!(
        io::stdout().lock(),
        "{{\"schema_version\":1,\"output_hex\":\"{output_hex}\",\
         \"samples_ns\":[{samples_json}]}}"
    )
    .map_err(|error| format!("failed to write result: {error}"))
}

/// Run a native workload executable.
pub fn main(workload: Workload) -> ExitCode {
    match command(workload) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("native benchmark: {error}");
            ExitCode::from(2)
        }
    }
}
