mod engine;

fn main() {
    std::process::exit(rv32vm_rust_common::command_main::<engine::Interpreter>());
}
