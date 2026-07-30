mod cli;
mod elf;
mod error;
mod machine;
mod memory;
mod protocol;

fn main() {
    std::process::exit(cli::main());
}
