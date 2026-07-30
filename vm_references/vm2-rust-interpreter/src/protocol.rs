//! Implements the persistent protocol defined by the `rv32vm` interface.

use std::io::{self, Read, Write};

use crate::machine::{LoadedProgram, MAX_INPUT_LENGTH, MAX_INSTRUCTION_LIMIT, MAX_OUTPUT_LIMIT};

const MAGIC: &[u8; 4] = b"RV32";
const VERSION: u8 = 1;
const HEADER_SIZE: usize = 16;
const RUN_HEADER_SIZE: usize = 16;
pub const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

const OP_LOAD: u8 = 1;
const OP_RUN: u8 = 2;
const OP_RESET: u8 = 3;
const OP_UNLOAD: u8 = 4;
const OP_SHUTDOWN: u8 = 5;

const OK: u16 = 0;
const MALFORMED_FRAME: u16 = 1;
const UNSUPPORTED_VERSION: u16 = 2;
const UNKNOWN_OPCODE: u16 = 3;
const INVALID_FLAGS: u16 = 4;
const FRAME_TOO_LARGE: u16 = 5;
const INVALID_PAYLOAD: u16 = 6;
const INVALID_STATE: u16 = 7;
const ELF_REJECTED: u16 = 8;

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ])
}

fn status_message(status: u16) -> &'static [u8] {
    match status {
        MALFORMED_FRAME => b"malformed frame",
        UNSUPPORTED_VERSION => b"unsupported version",
        UNKNOWN_OPCODE => b"unknown opcode",
        INVALID_FLAGS => b"invalid flags",
        FRAME_TOO_LARGE => b"frame too large",
        INVALID_PAYLOAD => b"invalid payload",
        INVALID_STATE => b"invalid state",
        ELF_REJECTED => b"ELF rejected",
        _ => b"internal error",
    }
}

fn read_available<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut received = 0;
    while received < buffer.len() {
        match reader.read(&mut buffer[received..])? {
            0 => break,
            count => received += count,
        }
    }
    Ok(received)
}

fn write_response<W: Write>(
    writer: &mut W,
    opcode: u8,
    request_id: u32,
    status: u16,
    payload: &[u8],
) -> io::Result<()> {
    let mut header = [0; HEADER_SIZE];
    header[..4].copy_from_slice(MAGIC);
    header[4] = VERSION;
    header[5] = opcode | 0x80;
    header[6..8].copy_from_slice(&status.to_le_bytes());
    header[8..12].copy_from_slice(&request_id.to_le_bytes());
    header[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()
}

fn write_error<W: Write>(
    writer: &mut W,
    opcode: u8,
    request_id: u32,
    status: u16,
) -> io::Result<()> {
    write_response(writer, opcode, request_id, status, status_message(status))
}

pub fn serve() -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_streams(&mut stdin.lock(), &mut stdout.lock())
}

fn serve_streams<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> Result<(), String> {
    write_response(writer, 0, 0, OK, &[]).map_err(|error| error.to_string())?;
    let mut program = None;

    loop {
        let mut header = [0; HEADER_SIZE];
        let received = read_available(reader, &mut header).map_err(|error| error.to_string())?;
        if received != HEADER_SIZE {
            if received >= 12 {
                write_error(writer, header[5], u32_at(&header, 8), MALFORMED_FRAME)
                    .map_err(|error| error.to_string())?;
            }
            return Err("incomplete protocol header".into());
        }

        let opcode = header[5];
        let request_id = u32_at(&header, 8);
        if &header[..4] != MAGIC {
            write_error(writer, opcode, request_id, MALFORMED_FRAME)
                .map_err(|error| error.to_string())?;
            return Err("invalid protocol magic".into());
        }
        let payload_length = u32_at(&header, 12) as usize;
        if payload_length > MAX_PAYLOAD {
            write_error(writer, opcode, request_id, FRAME_TOO_LARGE)
                .map_err(|error| error.to_string())?;
            return Err("protocol frame is too large".into());
        }

        let mut payload = vec![0; payload_length];
        if read_available(reader, &mut payload).map_err(|error| error.to_string())?
            != payload_length
        {
            write_error(writer, opcode, request_id, MALFORMED_FRAME)
                .map_err(|error| error.to_string())?;
            return Err("incomplete protocol payload".into());
        }

        if header[4] != VERSION {
            write_error(writer, opcode, request_id, UNSUPPORTED_VERSION)
                .map_err(|error| error.to_string())?;
            continue;
        }
        if u16_at(&header, 6) != 0 {
            write_error(writer, opcode, request_id, INVALID_FLAGS)
                .map_err(|error| error.to_string())?;
            continue;
        }
        if !matches!(
            opcode,
            OP_LOAD | OP_RUN | OP_RESET | OP_UNLOAD | OP_SHUTDOWN
        ) {
            write_error(writer, opcode, request_id, UNKNOWN_OPCODE)
                .map_err(|error| error.to_string())?;
            continue;
        }

        match opcode {
            OP_LOAD => {
                if payload.is_empty() {
                    write_error(writer, opcode, request_id, INVALID_PAYLOAD)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                if program.is_some() {
                    write_error(writer, opcode, request_id, INVALID_STATE)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                match LoadedProgram::new(&payload) {
                    Ok(loaded) => program = Some(loaded),
                    Err(_) => {
                        write_error(writer, opcode, request_id, ELF_REJECTED)
                            .map_err(|error| error.to_string())?;
                        continue;
                    }
                }
                write_response(writer, opcode, request_id, OK, &[])
                    .map_err(|error| error.to_string())?;
            }
            OP_RUN => {
                if payload.len() < RUN_HEADER_SIZE {
                    write_error(writer, opcode, request_id, INVALID_PAYLOAD)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let instruction_limit = u64_at(&payload, 0);
                let output_limit = u32_at(&payload, 8);
                let input_length = u32_at(&payload, 12) as usize;
                if payload.len() != RUN_HEADER_SIZE + input_length
                    || instruction_limit > MAX_INSTRUCTION_LIMIT
                    || output_limit > MAX_OUTPUT_LIMIT
                    || input_length > MAX_INPUT_LENGTH
                {
                    write_error(writer, opcode, request_id, INVALID_PAYLOAD)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let Some(program) = &program else {
                    write_error(writer, opcode, request_id, INVALID_STATE)
                        .map_err(|error| error.to_string())?;
                    continue;
                };
                let mut machine = program.machine(&payload[RUN_HEADER_SIZE..], output_limit);
                let result = machine.run(instruction_limit).json();
                let mut response = Vec::with_capacity(8 + result.len() + machine.output.len());
                response.extend_from_slice(&(result.len() as u32).to_le_bytes());
                response.extend_from_slice(&(machine.output.len() as u32).to_le_bytes());
                response.extend_from_slice(result.as_bytes());
                response.extend_from_slice(&machine.output);
                write_response(writer, opcode, request_id, OK, &response)
                    .map_err(|error| error.to_string())?;
            }
            OP_RESET => {
                if !payload.is_empty() {
                    write_error(writer, opcode, request_id, INVALID_PAYLOAD)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                if program.is_none() {
                    write_error(writer, opcode, request_id, INVALID_STATE)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                write_response(writer, opcode, request_id, OK, &[])
                    .map_err(|error| error.to_string())?;
            }
            OP_UNLOAD => {
                if !payload.is_empty() {
                    write_error(writer, opcode, request_id, INVALID_PAYLOAD)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                if program.is_none() {
                    write_error(writer, opcode, request_id, INVALID_STATE)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                program = None;
                write_response(writer, opcode, request_id, OK, &[])
                    .map_err(|error| error.to_string())?;
            }
            OP_SHUTDOWN => {
                if !payload.is_empty() {
                    write_error(writer, opcode, request_id, INVALID_PAYLOAD)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                write_response(writer, opcode, request_id, OK, &[])
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{HEADER_SIZE, MAGIC, OP_SHUTDOWN, VERSION, serve_streams};

    #[test]
    fn writes_ready_and_accepts_shutdown() {
        let mut request = [0; HEADER_SIZE];
        request[..4].copy_from_slice(MAGIC);
        request[4] = VERSION;
        request[5] = OP_SHUTDOWN;
        request[8..12].copy_from_slice(&7_u32.to_le_bytes());
        let mut output = Vec::new();

        serve_streams(&mut Cursor::new(request), &mut output).unwrap();

        assert_eq!(output.len(), HEADER_SIZE * 2);
        assert_eq!(&output[..4], MAGIC);
        assert_eq!(output[5], 0x80);
        assert_eq!(output[HEADER_SIZE + 5], OP_SHUTDOWN | 0x80);
        assert_eq!(u32::from_le_bytes(output[24..28].try_into().unwrap()), 7);
    }
}
