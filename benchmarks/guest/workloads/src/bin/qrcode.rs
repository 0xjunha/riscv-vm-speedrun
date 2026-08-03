#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! No-heap QR Code encoding of a provisioning payload.

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::{crc32, encode_output, Words};

use qrcodegen::{QrCode, QrCodeEcc, Version};

const MAX_VERSION: u8 = 20;
const MAX_SIZE: usize = MAX_VERSION as usize * 4 + 17;
const BUFFER_SIZE: usize = (MAX_SIZE * MAX_SIZE).div_ceil(8) + 1;
const MODULE_BYTES: usize = (MAX_SIZE * MAX_SIZE).div_ceil(8);

fn qrcode(input: &[u8]) -> [u8; 16] {
    let length = Words::new(input).get(0) as usize;
    if length > BUFFER_SIZE {
        return encode_output(17, 0, 0);
    }
    let Some(payload) = input.get(4..4usize.saturating_add(length)) else {
        return encode_output(17, 0, 0);
    };

    let mut temporary = [0u8; BUFFER_SIZE];
    let mut output = [0u8; BUFFER_SIZE];
    temporary[..length].copy_from_slice(payload);
    let Ok(code) = QrCode::encode_binary(
        &mut temporary,
        length,
        &mut output,
        QrCodeEcc::Medium,
        Version::MIN,
        Version::new(MAX_VERSION),
        None,
        true,
    ) else {
        return encode_output(17, 0, 0);
    };

    let size = code.size() as usize;
    let mut modules = [0u8; MODULE_BYTES];
    let mut dark = 0u32;
    for y in 0..size {
        for x in 0..size {
            if code.get_module(x as i32, y as i32) {
                let index = y * size + x;
                modules[index / 8] |= 1 << (index & 7);
                dark += 1;
            }
        }
    }
    let used = (size * size).div_ceil(8);
    let metadata = (u32::from(code.version().value()) << 24) | size as u32;
    encode_output(17, dark, crc32(&modules[..used]) ^ metadata)
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(qrcode, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(qrcode, input))
}
