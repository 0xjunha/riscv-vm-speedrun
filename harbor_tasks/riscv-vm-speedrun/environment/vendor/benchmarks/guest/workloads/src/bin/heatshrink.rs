#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! Heatshrink compression and decompression of embedded telemetry records.

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::{crc32, encode_result, join_u32};

const MAX_PAYLOAD: usize = 16 * 1024;
const MAX_COMPRESSED: usize = MAX_PAYLOAD + MAX_PAYLOAD / 8 + 32;

fn heatshrink(input: &[u8]) -> [u8; 8] {
    if input.len() > MAX_PAYLOAD {
        return encode_result(0);
    }

    let mut compressed = [0u8; MAX_COMPRESSED];
    let Ok(encoded) = heatshrink::encoder::encode(input, &mut compressed) else {
        return encode_result(0);
    };
    let encoded_length = encoded.len() as u32;
    let encoded_crc = crc32(encoded);

    let mut decoded = [0u8; MAX_PAYLOAD];
    let Ok(decoded) = heatshrink::decoder::decode(encoded, &mut decoded) else {
        return encode_result(0);
    };
    if decoded != input {
        return encode_result(0);
    }
    let decoded_summary = crc32(decoded) ^ encoded_length.rotate_left(16);
    encode_result(join_u32(encoded_crc, decoded_summary))
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(heatshrink, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(heatshrink, input))
}
