#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! Integer depthwise convolution adapted from TensorFlow Lite Micro via Embench.

// Copyright 2024 The TensorFlow Authors. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0
// Modified for this benchmark as described in `THIRD_PARTY_NOTICES.md`.

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::{crc32, encode_result, join_u32, Words};

const HEIGHT: usize = 16;
const WIDTH: usize = 16;
const CHANNELS: usize = 8;
const FILTER: usize = 3;
const ACTIVATIONS: usize = HEIGHT * WIDTH * CHANNELS;
const WEIGHTS: usize = FILTER * FILTER * CHANNELS;
const HEADER: usize = 4;
const ACTIVATION_OFFSET: usize = HEADER;
const WEIGHT_OFFSET: usize = ACTIVATION_OFFSET + ACTIVATIONS;
const BIAS_OFFSET: usize = WEIGHT_OFFSET + WEIGHTS;
const MULTIPLIER_OFFSET: usize = BIAS_OFFSET + CHANNELS * 4;
const SHIFT_OFFSET: usize = MULTIPLIER_OFFSET + CHANNELS * 4;
const INPUT_SIZE: usize = SHIFT_OFFSET + CHANNELS * 4;

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn multiply_by_quantized_multiplier(value: i32, multiplier: i32, shift: i32) -> i32 {
    let total_shift = 31 - i64::from(shift);
    let rounding = 1i64 << (total_shift - 1);
    ((i64::from(value) * i64::from(multiplier) + rounding) >> total_shift) as i32
}

#[inline(never)]
fn convolve(
    activations: &[u8],
    weights: &[u8],
    biases: &[i32; CHANNELS],
    multipliers: &[i32; CHANNELS],
    shifts: &[i32; CHANNELS],
    output: &mut [u8; ACTIVATIONS],
) {
    for out_y in 0..HEIGHT {
        for out_x in 0..WIDTH {
            for channel in 0..CHANNELS {
                let mut accumulator = biases[channel];
                for filter_y in 0..FILTER {
                    let in_y = out_y as isize + filter_y as isize - 1;
                    if !(0..HEIGHT as isize).contains(&in_y) {
                        continue;
                    }
                    for filter_x in 0..FILTER {
                        let in_x = out_x as isize + filter_x as isize - 1;
                        if !(0..WIDTH as isize).contains(&in_x) {
                            continue;
                        }
                        let input_index =
                            (in_y as usize * WIDTH + in_x as usize) * CHANNELS + channel;
                        let weight_index = (filter_y * FILTER + filter_x) * CHANNELS + channel;
                        let input_value = i32::from(activations[input_index] as i8) + 3;
                        let weight = i32::from(weights[weight_index] as i8);
                        accumulator = accumulator.wrapping_add(weight.wrapping_mul(input_value));
                    }
                }
                let quantized = multiply_by_quantized_multiplier(
                    accumulator,
                    multipliers[channel],
                    shifts[channel],
                )
                .saturating_sub(2)
                .clamp(-128, 127);
                output[(out_y * WIDTH + out_x) * CHANNELS + channel] = quantized as i8 as u8;
            }
        }
    }
}

fn depthconv(input: &[u8]) -> [u8; 8] {
    if input.len() != INPUT_SIZE {
        return encode_result(0);
    }
    let repetitions = Words::new(input).get(0).clamp(1, 32);
    let activations = &input[ACTIVATION_OFFSET..WEIGHT_OFFSET];
    let weights = &input[WEIGHT_OFFSET..BIAS_OFFSET];
    let mut biases = [0i32; CHANNELS];
    let mut multipliers = [0i32; CHANNELS];
    let mut shifts = [0i32; CHANNELS];
    for channel in 0..CHANNELS {
        biases[channel] = read_i32(input, BIAS_OFFSET + channel * 4);
        multipliers[channel] = read_i32(input, MULTIPLIER_OFFSET + channel * 4);
        shifts[channel] = read_i32(input, SHIFT_OFFSET + channel * 4);
    }

    let mut output = [0u8; ACTIVATIONS];
    let mut aggregate = 0u32;
    for pass in 0..repetitions {
        convolve(
            activations,
            weights,
            &biases,
            &multipliers,
            &shifts,
            &mut output,
        );
        aggregate ^= crc32(&output).rotate_left(pass & 31);
    }
    encode_result(join_u32(aggregate, crc32(&output)))
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(depthconv, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(depthconv, input))
}
