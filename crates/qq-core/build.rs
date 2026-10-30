use std::{env, fs, path::Path, process::Command};

use sha2::{Digest as _, Sha256};

fn main() {
    build_watcher();
    let root = Path::new("../..");
    let mut paths = vec![
        root.join("Cargo.lock"),
        root.join("Cargo.toml"),
        root.join("crates/qq-host/src/runtime.rs"),
        root.join("build.rs"),
    ];
    collect(Path::new("src"), &mut paths);
    collect(&root.join("crates/qq-protocol/src"), &mut paths);
    paths.extend(
        [
            "build.rs",
            "Cargo.toml",
            "helper/Cargo.toml",
            "helper/Cargo.lock",
            "helper/src/main.rs",
        ]
        .map(std::path::PathBuf::from),
    );
    paths.sort();
    let mut digest = Sha256::new();
    digest.update(b"qq-process-context-policy-v2\0");
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.to_string_lossy();
        let bytes = fs::read(&path).expect("process policy build input is readable");
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    let mut settings: Vec<_> = env::vars()
        .filter(|(key, _)| key == "TARGET" || key.starts_with("CARGO_FEATURE_"))
        .collect();
    settings.sort();
    for (key, value) in settings {
        digest.update((key.len() as u64).to_le_bytes());
        digest.update(key.as_bytes());
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    println!(
        "cargo:rustc-env=QQ_PROCESS_POLICY_SHA256={:x}",
        digest.finalize()
    );
}

fn build_watcher() {
    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR is set during build");
    let embed = Path::new(&out_dir).join("qq-command-watcher");
    let helper = Path::new("helper");
    println!(
        "cargo:rerun-if-changed={}",
        helper.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        helper.join("src/main.rs").display()
    );

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" && target_os != "macos" {
        fs::write(&embed, []).expect("write placeholder watcher image");
        return;
    }

    let target = env::var("TARGET").expect("TARGET is set during build");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let helper_target = Path::new(&out_dir).join("watcher-target");
    let status = Command::new(&cargo)
        .current_dir(helper)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env("CARGO_TARGET_DIR", &helper_target)
        .args(["build", "--locked", "--quiet", "--target", &target])
        .status();
    let built = helper_target
        .join(&target)
        .join("debug")
        .join("qq-command-watcher");
    match status {
        Ok(status) if status.success() && built.is_file() => {
            fs::copy(&built, &embed).expect("copy watcher image into OUT_DIR");
        }
        _ => panic!("the command watcher helper failed to build"),
    }
}

fn collect(directory: &Path, paths: &mut Vec<std::path::PathBuf>) {
    println!("cargo:rerun-if-changed={}", directory.display());
    for entry in fs::read_dir(directory).expect("process policy source directory is readable") {
        let path = entry
            .expect("process policy source entry is readable")
            .path();
        if path.is_dir() {
            collect(&path, paths);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.push(path);
        }
    }
}
