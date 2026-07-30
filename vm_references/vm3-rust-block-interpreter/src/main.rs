mod block;
mod cache;
mod engine;

#[cfg(test)]
mod test_support;

fn main() {
    std::process::exit(rv32vm_rust_common::command_main::<engine::BlockInterpreter>());
}
