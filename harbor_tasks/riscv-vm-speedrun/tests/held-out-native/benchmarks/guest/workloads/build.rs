use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CLANG_ENV: &str = "RVB_C_CLANG";
const C_WORKLOADS_ENV: &str = "RVB_C_WORKLOADS";
const LLVM_AR_ENV: &str = "RVB_C_LLVM_AR";
const C_WORKLOADS_FEATURE_ENV: &str = "CARGO_FEATURE_C_WORKLOADS";

const LITTLEFS_DEFINES: &[&str] = &[
    "LFS_NO_MALLOC",
    "LFS_NO_ASSERT",
    "LFS_NO_DEBUG",
    "LFS_NO_WARN",
    "LFS_NO_ERROR",
];
const STATEMATE_DEFINES: &[&str] = &[
    "benchmark=embench_statemate_benchmark",
    "warm_caches=embench_statemate_warm_caches",
    "initialise_benchmark=embench_statemate_initialise_benchmark",
    "verify_benchmark=embench_statemate_verify_benchmark",
];
const C_ADAPTERS: &[(&str, WarningPolicy, &[&str])] = &[
    ("littlefs", WarningPolicy::Strict, LITTLEFS_DEFINES),
    ("x25519", WarningPolicy::Strict, &[]),
    ("aes", WarningPolicy::Strict, &[]),
    ("mont64", WarningPolicy::Strict, &[]),
    ("picojpeg", WarningPolicy::Strict, &[]),
    // The upstream macro expansion triggers unused-local warnings.
    ("sglib", WarningPolicy::Upstream, &[]),
    ("slre", WarningPolicy::Strict, &[]),
    ("statemate", WarningPolicy::Strict, &[]),
    ("ud", WarningPolicy::Strict, &[]),
];
const C_UPSTREAM_SOURCES: &[(&str, &str, &[&str])] = &[
    ("littlefs", "littlefs/lfs.c", LITTLEFS_DEFINES),
    ("littlefs", "littlefs/lfs_util.c", LITTLEFS_DEFINES),
    ("x25519", "monocypher/monocypher.c", &[]),
    ("mont64", "embench/aha-mont64/mont64.c", &[]),
    ("aes", "embench/nettle-aes/nettle-aes.c", &[]),
    ("picojpeg", "embench/picojpeg/libpicojpeg.c", &[]),
    ("slre", "embench/slre/libslre.c", &[]),
    (
        "statemate",
        "embench/statemate/libstatemate.c",
        STATEMATE_DEFINES,
    ),
    ("ud", "embench/ud/libud.c", &[]),
];
const C_INCLUDE_DIRECTORIES: &[(&str, &str)] = &[
    ("littlefs", "littlefs"),
    ("x25519", "monocypher"),
    ("picojpeg", "embench/picojpeg"),
    ("sglib", "embench/sglib-combined"),
    ("slre", "embench/slre"),
];

#[derive(Clone, Copy)]
enum WarningPolicy {
    Strict,
    Upstream,
}

struct CSource {
    path: PathBuf,
    warning_policy: WarningPolicy,
    defines: &'static [&'static str],
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let script = manifest.join("../link.x");
    println!("cargo:rerun-if-changed={}", script.display());

    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("riscv32") {
        println!("cargo:rustc-link-arg=-T{}", script.display());
    }

    if env::var_os(C_WORKLOADS_FEATURE_ENV).is_some() {
        compile_c_workloads(&manifest);
    }
}

fn compile_c_workloads(manifest: &Path) {
    let third_party = manifest.join("../../third_party");
    let source_root = manifest.join("c");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let (target_flags, freestanding): (&[&str], bool) =
        match (target_arch.as_str(), target_os.as_str()) {
            ("riscv32", "none") => (
                &[
                    "--target=riscv32-unknown-none-elf",
                    "-march=rv32im",
                    "-mabi=ilp32",
                    "-mno-relax",
                ],
                true,
            ),
            ("x86_64", "linux") => (&["--target=x86_64-unknown-linux-gnu"], false),
            _ => panic!("C workloads do not support Cargo target {target_arch}-{target_os}"),
        };
    let selection = env::var(C_WORKLOADS_ENV).ok();
    let selected = |name: &str| {
        selection
            .as_deref()
            .is_none_or(|names| names.split_ascii_whitespace().any(|item| item == name))
    };
    let mut sources = Vec::new();
    let mut include_directories = vec![source_root.join("include")];
    sources.extend(
        C_ADAPTERS
            .iter()
            .filter(|(workload, _, _)| selected(workload))
            .map(|(workload, warning_policy, defines)| CSource {
                path: source_root.join(format!("adapters/{workload}.c")),
                warning_policy: *warning_policy,
                defines,
            }),
    );
    sources.extend(
        C_UPSTREAM_SOURCES
            .iter()
            .filter(|(workload, _, _)| selected(workload))
            .map(|(_, path, defines)| CSource {
                path: third_party.join(path),
                warning_policy: WarningPolicy::Upstream,
                defines,
            }),
    );
    include_directories.extend(
        C_INCLUDE_DIRECTORIES
            .iter()
            .filter(|(workload, _)| selected(workload))
            .map(|(_, path)| third_party.join(path)),
    );
    println!("cargo:rerun-if-env-changed={C_WORKLOADS_ENV}");
    if sources.is_empty() {
        return;
    }
    if freestanding {
        sources.insert(
            1,
            CSource {
                path: source_root.join("c_compat.c"),
                warning_policy: WarningPolicy::Strict,
                defines: &[],
            },
        );
    }
    println!("cargo:rerun-if-env-changed={CLANG_ENV}");
    println!("cargo:rerun-if-env-changed={LLVM_AR_ENV}");
    emit_authored_inputs(&source_root);
    for root in include_directories.iter().skip(1) {
        emit_authored_inputs(root);
    }
    for source in &sources {
        println!("cargo:rerun-if-changed={}", source.path.display());
    }

    let clang = env::var_os(CLANG_ENV).unwrap_or_else(|| OsStr::new("clang-14").into());
    let llvm_ar = env::var_os(LLVM_AR_ENV).unwrap_or_else(|| OsStr::new("llvm-ar-14").into());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let mut objects = Vec::with_capacity(sources.len());

    for (index, source) in sources.iter().enumerate() {
        let stem = source
            .path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("source");
        let object = out_dir.join(format!("{index:02}-{stem}.o"));
        let mut command = Command::new(&clang);
        command
            .args(target_flags)
            .args([
                "-std=c99",
                "-O3",
                "-ffreestanding",
                "-fno-builtin",
                "-fno-ident",
                "-fno-stack-protector",
                "-fdata-sections",
                "-ffunction-sections",
            ])
            .arg("-c")
            .arg(&source.path)
            .arg("-o")
            .arg(&object);
        if freestanding {
            for include in &include_directories {
                command.arg("-I").arg(include);
            }
        } else {
            // Keep the project adapter header available for quoted includes,
            // while angle-bracket C library headers resolve to the host libc.
            command.arg("-iquote").arg(&include_directories[0]);
            for include in include_directories.iter().skip(1) {
                command.arg("-I").arg(include);
            }
        }
        if matches!(source.warning_policy, WarningPolicy::Strict) {
            command.args(["-Wall", "-Wextra", "-Wpedantic", "-Werror", "-Wdate-time"]);
        }
        for define in source.defines {
            command.arg(format!("-D{define}"));
        }
        run(&mut command, "C compilation");
        objects.push(object);
    }

    let archive = out_dir.join("librvb_c_workloads.a");
    if let Err(error) = fs::remove_file(&archive) {
        if error.kind() != std::io::ErrorKind::NotFound {
            panic!("cannot replace {}: {error}", archive.display());
        }
    }
    let mut command = Command::new(&llvm_ar);
    command.arg("rcsD").arg(&archive).args(&objects);
    run(&mut command, "C archive creation");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
}

fn emit_authored_inputs(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let mut entries: Vec<_> = fs::read_dir(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                pending.push(entry);
            } else {
                println!("cargo:rerun-if-changed={}", entry.display());
            }
        }
    }
}

fn run(command: &mut Command, action: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("cannot run {action} with {command:?}: {error}"));
    if !status.success() {
        panic!("{action} failed with {status}: {command:?}");
    }
}
