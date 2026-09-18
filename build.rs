//! Embeds the build's source revision into `qq --version`.
//!
//! The version string is `<crate version> (<short sha> <commit date>)`, with a
//! `-dirty` suffix when the worktree had uncommitted changes. Release builds
//! from a tarball have no `.git`; they read `QQ_GIT_SHA` / `QQ_GIT_DATE` from
//! the environment (the release workflow exports them) and otherwise print
//! `unknown`. The build never fails because of revision lookup.

use std::{env, path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=QQ_GIT_SHA");
    println!("cargo:rerun-if-env-changed=QQ_GIT_DATE");

    let (build_revision, source_revision, date) =
        match (env::var("QQ_GIT_SHA"), env::var("QQ_GIT_DATE")) {
            (Ok(sha), Ok(date)) if !sha.is_empty() && !date.is_empty() => (sha.clone(), sha, date),
            _ => match from_git() {
                Some(found) => found,
                None => (
                    "unknown".to_owned(),
                    "unknown".to_owned(),
                    "unknown".to_owned(),
                ),
            },
        };

    println!("cargo:rustc-env=QQ_BUILD_REVISION={build_revision}");
    println!("cargo:rustc-env=QQ_SOURCE_REVISION={source_revision}");
    println!("cargo:rustc-env=QQ_BUILD_DATE={date}");
}

/// Short display SHA, full source SHA (both with `-dirty` when the tree is
/// modified), and ISO commit date.
fn from_git() -> Option<(String, String, String)> {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")?;
    let root = Path::new(&manifest_dir);

    // Rebuild when the checked-out commit changes so a stale revision is never
    // baked into an incremental build.
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-path", "HEAD"])
        .current_dir(root)
        .output()
        && output.status.success()
    {
        let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        println!("cargo:rerun-if-changed={head}");
        // A symbolic HEAD moves when its branch does; watch the loose ref only
        // when it exists (a missing path would make Cargo rerun every build).
        if let Ok(target) = std::fs::read_to_string(root.join(&head))
            && let Some(reference) = target.strip_prefix("ref: ")
            && let Ok(output) = Command::new("git")
                .args(["rev-parse", "--git-path", reference.trim()])
                .current_dir(root)
                .output()
            && output.status.success()
        {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if root.join(&path).is_file() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    // `git describe --dirty` covers tracked changes only. Watch the index and
    // every tracked path so an incremental build cannot retain a clean source
    // revision after one of those inputs becomes dirty (or vice versa).
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-path", "index"])
        .current_dir(root)
        .output()
        && output.status.success()
    {
        let index = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if root.join(&index).is_file() {
            println!("cargo:rerun-if-changed={index}");
        }
    }
    if let Ok(output) = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        && output.status.success()
    {
        for path in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = String::from_utf8_lossy(path);
            if !path.contains('\n') && !path.contains('\r') {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }

    let describe = Command::new("git")
        .args([
            "describe",
            "--always",
            "--dirty=-dirty",
            "--abbrev=7",
            "--exclude=*",
        ])
        .current_dir(root)
        .output()
        .ok()?;
    if !describe.status.success() {
        return None;
    }
    let build_revision = String::from_utf8_lossy(&describe.stdout).trim().to_owned();

    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if !revision.status.success() {
        return None;
    }
    let mut source_revision = String::from_utf8_lossy(&revision.stdout).trim().to_owned();
    if build_revision.ends_with("-dirty") {
        source_revision.push_str("-dirty");
    }

    let date = Command::new("git")
        .args(["log", "-1", "--format=%cs"])
        .current_dir(root)
        .output()
        .ok()?;
    if !date.status.success() {
        return None;
    }
    let date = String::from_utf8_lossy(&date.stdout).trim().to_owned();

    if build_revision.is_empty() || source_revision.is_empty() || date.is_empty() {
        return None;
    }
    Some((build_revision, source_revision, date))
}
