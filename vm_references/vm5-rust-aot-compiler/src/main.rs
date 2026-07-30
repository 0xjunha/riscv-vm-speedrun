#[cfg(any(
    test,
    all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    )
))]
mod aot;
#[cfg(any(
    test,
    all(
        target_arch = "x86_64",
        target_os = "linux",
        target_pointer_width = "64"
    )
))]
mod engine;

#[cfg(test)]
mod test_support;

#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
))]
fn main() {
    std::process::exit(rv32vm_rust_common::command_main::<engine::AotCompiler>());
}

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_pointer_width = "64"
)))]
fn main() {
    eprintln!("rv32vm: VM5 AOT compiler requires x86-64 Linux");
    std::process::exit(2);
}
