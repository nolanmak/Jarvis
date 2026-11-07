use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/ci_cargo_exec.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_relay.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_status_wire.h");
    println!("cargo:rerun-if-changed=src/ci_cargo_status_proxy.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_status_continuation.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_status_continuation.h");
    println!("cargo:rerun-if-changed=src/ci_cargo_status_vfork_producer.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_exec_early.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_exec_static.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_vfork_producer.c");
    println!("cargo:rerun-if-changed=src/ci_cargo_vfork_proxy.c");
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
        fs::write(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("libci_cargo_planner.so"), [])
            .expect("write unsupported planner marker");
        for name in ["libci_cargo_exec_early.so", "ci_cargo_exec_static", "ci_cargo_vfork_producer", "ci_cargo_vfork_proxy", "ci_cargo_status_proxy", "ci_cargo_status_vfork_producer"] {
            fs::write(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join(name), [])
                .expect("write unsupported exec feasibility marker");
        }
        return;
    }
    let source = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("src/ci_cargo_exec.c");
    let status = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-shared", "-fPIC", "-std=c11", "-O2", "-g0", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_status_continuation.c"))
        .arg("-o")
        .arg(output)
        .status()
        .expect("start native helper compiler");
    assert!(status.success(), "native exec-denial helper compilation failed");
    let status=Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-shared", "-fPIC", "-std=c11", "-O2", "-g0", "-Wall", "-Wextra", "-Werror", "-Wl,-z,now"])
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_relay.c"))
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_exec.c"))
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_status_continuation.c"))
        .arg("-o").arg(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("libci_cargo_relay.so"))
        .status().expect("start relay compiler");
    assert!(status.success(),"native relay compilation failed");
    let status=Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-shared", "-fPIC", "-std=c11", "-O2", "-g0", "-Wall", "-Wextra", "-Werror", "-Wl,-z,now", "-DJARVIS_CARGO_PROTOCOL_V3=1"])
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_relay.c"))
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_exec.c"))
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_status_continuation.c"))
        .arg("-o").arg(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("libci_cargo_planner.so"))
        .status().expect("start protected planner relay compiler");
    assert!(status.success(),"native protected planner relay compilation failed");
    let status=Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-std=c11","-O2","-g0","-Wall","-Wextra","-Werror","-static","-fno-pie","-no-pie"])
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_status_proxy.c"))
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_exec.c"))
        .arg("-o").arg(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ci_cargo_status_proxy"))
        .status().expect("start V3 status proxy compiler");
    assert!(status.success(),"native V3 status proxy compilation failed");
    let status=Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args(["-std=c11","-O2","-g0","-Wall","-Wextra","-Werror"])
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_status_vfork_producer.c"))
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_exec.c"))
        .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/ci_cargo_status_continuation.c"))
        .arg("-o").arg(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ci_cargo_status_vfork_producer"))
        .status().expect("start combined V3 vfork producer compiler");
    assert!(status.success(),"combined V3 vfork producer compilation failed");
    for (source, output, flags) in [
        ("ci_cargo_exec_early.c", "libci_cargo_exec_early.so", vec!["-shared", "-fPIC"]),
        ("ci_cargo_exec_static.c", "ci_cargo_exec_static", vec!["-nostdlib", "-static", "-fno-stack-protector",
            "-fno-pie", "-no-pie", "-mno-sse", "-mno-mmx", "-fno-builtin"]),
        ("ci_cargo_vfork_producer.c", "ci_cargo_vfork_producer", vec![]),
        ("ci_cargo_vfork_proxy.c", "ci_cargo_vfork_proxy", vec!["-nostdlib", "-static", "-fno-stack-protector",
            "-fno-pie", "-no-pie", "-mno-sse", "-mno-mmx", "-fno-builtin"]),
    ] {
        let status = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-std=c11", "-O2", "-g0", "-Wall", "-Wextra", "-Werror"])
            .args(flags)
            .arg(PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src").join(source))
            .arg("-o").arg(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join(output))
            .status().expect("start exec feasibility fixture compiler");
        assert!(status.success(), "native exec feasibility fixture compilation failed");
    }
}
