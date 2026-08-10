#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! Stable sorting of bounded key/value records.

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::{encode_result, join_u32, Words};

const MAX_RECORDS: usize = 2_048;

#[derive(Clone, Copy, Default)]
struct Record {
    key: u32,
    value: u32,
}

fn before_or_equal(left: Record, right: Record) -> bool {
    left.key <= right.key
}

#[inline(never)]
fn merge_sort(
    records: &mut [Record; MAX_RECORDS],
    scratch: &mut [Record; MAX_RECORDS],
    len: usize,
) {
    let mut width = 1;
    while width < len {
        let mut start = 0;
        while start < len {
            let middle = (start + width).min(len);
            let end = (start + width * 2).min(len);
            let (mut left, mut right, mut output) = (start, middle, start);
            while left < middle && right < end {
                if before_or_equal(records[left], records[right]) {
                    scratch[output] = records[left];
                    left += 1;
                } else {
                    scratch[output] = records[right];
                    right += 1;
                }
                output += 1;
            }
            while left < middle {
                scratch[output] = records[left];
                left += 1;
                output += 1;
            }
            while right < end {
                scratch[output] = records[right];
                right += 1;
                output += 1;
            }
            start = end;
        }
        records[..len].copy_from_slice(&scratch[..len]);
        width *= 2;
    }
}

fn sort_records(input: &[u8]) -> [u8; 8] {
    let words = Words::new(input);
    let passes = words.get(0).clamp(1, 16);
    let count = words.word_count().saturating_sub(1) / 2;
    if !(2..=MAX_RECORDS).contains(&count) || words.word_count() != 1 + count * 2 {
        return encode_result(0);
    }

    let mut records = [Record::default(); MAX_RECORDS];
    let mut scratch = [Record::default(); MAX_RECORDS];
    let mut aggregate = 0u32;
    let mut final_fold = 0u32;
    for pass in 0..passes {
        let key_mask = pass.wrapping_mul(0x9e37_79b9);
        for (index, record) in records[..count].iter_mut().enumerate() {
            *record = Record {
                key: words.get(1 + index * 2) ^ key_mask,
                value: words.get(2 + index * 2),
            };
        }
        merge_sort(&mut records, &mut scratch, count);

        let mut fold = 0x811c_9dc5u32;
        for record in &records[..count] {
            fold = fold.rotate_left(5) ^ record.key;
            fold = fold.wrapping_mul(0x0100_0193) ^ record.value;
        }
        aggregate ^= fold.rotate_left(pass & 31);
        final_fold = fold;
    }
    encode_result(join_u32(aggregate, final_fold))
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(sort_records, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(sort_records, input))
}
