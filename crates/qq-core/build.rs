use std::{env, fs, path::Path};

use sha2::{Digest as _, Sha256};

fn main() {
    let root = Path::new("../..");
    let mut paths = vec![
        root.join("Cargo.lock"),
        root.join("Cargo.toml"),
        root.join("src/runtime.rs"),
        root.join("build.rs"),
    ];
    collect(Path::new("src"), &mut paths);
    collect(&root.join("crates/qq-protocol/src"), &mut paths);
    paths.extend(["build.rs", "Cargo.toml"].map(std::path::PathBuf::from));
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
