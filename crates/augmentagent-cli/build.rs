use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/ci_cargo_exec.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_relay.c");
    println!("cargo:rerun-if-env-changed=CC");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"))
        .join("libci_cargo_exec.so");
    let supported = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
        && env::var("HOST").ok() == env::var("TARGET").ok();
    if !supported {
        // The experiment has no runtime activation on other targets. Keep
        // ordinary CLI builds available; unsupported probes refuse separately.
        fs::write(output, []).expect("write unsupported native helper marker");
        fs::write(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("libci_cargo_relay.so"), [])
            .expect("write unsupported relay marker");
        return;
    }
    let source = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("src/ci_cargo_exec.c");
    let status = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-shared", "-fPIC", "-std=c11", "-O2", "-g0", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(output)
        .status()
        .expect("start native helper compiler");
    assert!(status.success(), "native exec-denial helper compilation failed");
    let status=Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-shared", "-fPIC", "-std=c11", "-O2", "-g0", "-Wall", "-Wextra", "-Werror", "-Wl,-z,now"])
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_relay.c"))
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_exec.c"))
        .arg("-o").arg(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("libci_cargo_relay.so"))
        .status().expect("start relay compiler");
    assert!(status.success(),"native relay compilation failed");
}
