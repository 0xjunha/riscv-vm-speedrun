#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

//! Shortest paths over a fixed-capacity weighted graph.

#[cfg(target_os = "none")]
use rv32im_guest::guest_entry;
use rv32im_workloads::{encode_output, Words};

const MAX_NODES: usize = 64;

fn edge(input: &[u8], nodes: usize, from: usize, to: usize) -> u32 {
    let offset = 8 + (from * nodes + to) * 2;
    u32::from(u16::from_le_bytes([input[offset], input[offset + 1]]))
}

fn dijkstra(input: &[u8]) -> [u8; 12] {
    let words = Words::new(input);
    let nodes = words.get(0) as usize;
    let sources = words.get(1) as usize;
    let matrix_bytes = match nodes
        .checked_mul(nodes)
        .and_then(|value| value.checked_mul(2))
    {
        Some(value) => value,
        None => return encode_output(0, 0),
    };
    if !(2..=MAX_NODES).contains(&nodes)
        || sources == 0
        || sources > nodes
        || input.len() != 8 + matrix_bytes
    {
        return encode_output(0, 0);
    }

    let mut distances = [u32::MAX; MAX_NODES];
    let mut visited = [false; MAX_NODES];
    let mut total = 0u32;
    let mut fold = 0u32;

    for source in 0..sources {
        distances[..nodes].fill(u32::MAX);
        visited[..nodes].fill(false);
        distances[source] = 0;

        for _ in 0..nodes {
            let mut selected = nodes;
            let mut minimum = u32::MAX;
            for node in 0..nodes {
                if !visited[node] && distances[node] < minimum {
                    minimum = distances[node];
                    selected = node;
                }
            }
            if selected == nodes {
                break;
            }
            visited[selected] = true;
            for target in 0..nodes {
                let weight = edge(input, nodes, selected, target);
                if weight == 0 || visited[target] {
                    continue;
                }
                let candidate = minimum.saturating_add(weight);
                if candidate < distances[target] {
                    distances[target] = candidate;
                }
            }
        }

        for (node, &distance) in distances[..nodes].iter().enumerate() {
            total = total.wrapping_add(distance);
            fold ^= distance.rotate_left(((node + source) & 31) as u32);
        }
    }
    encode_output(total, fold)
}

#[cfg(target_os = "none")]
fn guest_main(input: &[u8]) -> u32 {
    rv32im_workloads::emit(&rv32im_workloads::run(dijkstra, input))
}

#[cfg(target_os = "none")]
guest_entry!(guest_main);

#[cfg(not(target_os = "none"))]
fn main() -> std::process::ExitCode {
    rv32im_workloads::native::main(|input| rv32im_workloads::run(dijkstra, input))
}
