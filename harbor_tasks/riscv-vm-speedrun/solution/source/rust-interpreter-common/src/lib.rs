mod cli;
mod elf;
mod error;
pub mod machine;
pub mod memory;
mod protocol;

pub use error::GuestTrap;
pub use machine::Engine;

pub fn command_main<E: Engine + Default>() -> i32 {
    cli::main::<E>()
}
