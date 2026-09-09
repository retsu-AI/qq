//! `cargo xtask release X.Y.Z`: bump the workspace version, commit, and tag.
//!
//! The release workflow (`.github/workflows/release.yml`) refuses a `vX.Y.Z`
//! tag whose version differs from `[workspace.package] version`, so the bump
//! and the tag are produced together here. Nothing is pushed; the caller
//! reviews and pushes `main --follow-tags`.

use std::{
    env, fmt, io,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitStatus, Stdio},
};

use clap::Args;
use thiserror::Error;

#[derive(Debug, Args)]
pub struct ReleaseArgs {
    /// Version to release, e.g. `0.2.0`. Must be greater than the current
    /// workspace version.
    version: String,
    /// Update `Cargo.toml` and `Cargo.lock` but do not commit or tag.
    #[arg(long)]
    no_commit: bool,
}

#[derive(Debug, Error)]
pub enum ReleaseError {
    #[error("{0:?} is not a plain MAJOR.MINOR.PATCH version")]
    InvalidVersion(String),
    #[error("{requested} is not greater than the current version {current}")]
    NotGreater {
        requested: Version,
        current: Version,
    },
    #[error("release must run at the repository root; `Cargo.toml` is missing at {0}")]
    NotRepositoryRoot(PathBuf),
    #[error("failed to read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`[workspace.package] version = \"...\"` not found in Cargo.toml")]
    VersionLineMissing,
    #[error("the worktree has uncommitted changes; commit or stash them first")]
    DirtyWorktree,
    #[error("tag v{0} already exists")]
    TagExists(Version),
    #[error("failed to launch `{program}`")]
    Launch {
        program: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("`{program} {args}` exited with {status}")]
    Failed {
        program: &'static str,
        args: String,
        status: ExitStatus,
    },
    #[error("release task stopped unexpectedly")]
    Task(#[source] tokio::task::JoinError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Accepts only `MAJOR.MINOR.PATCH`; pre-release and build metadata are
    /// out of scope for this tool.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub async fn run(args: ReleaseArgs) -> Result<(), ReleaseError> {
    tokio::task::spawn_blocking(move || run_blocking(args))
        .await
        .map_err(ReleaseError::Task)?
}

fn run_blocking(args: ReleaseArgs) -> Result<(), ReleaseError> {
    let requested = Version::parse(&args.version)
        .ok_or_else(|| ReleaseError::InvalidVersion(args.version.clone()))?;

    let root = env::current_dir().map_err(|source| ReleaseError::Read {
        path: PathBuf::from("."),
        source,
    })?;
    let manifest_path = root.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Err(ReleaseError::NotRepositoryRoot(root));
    }

    if !args.no_commit {
        let status = git(&root, &["diff", "--quiet", "HEAD", "--"], Stdio::inherit())?;
        if !status.success() {
            return Err(ReleaseError::DirtyWorktree);
        }
        let tag = format!("v{requested}");
        let status = git(
            &root,
            &["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")],
            Stdio::null(),
        )?;
        if status.success() {
            return Err(ReleaseError::TagExists(requested));
        }
    }

    let manifest =
        std::fs::read_to_string(&manifest_path).map_err(|source| ReleaseError::Read {
            path: manifest_path.clone(),
            source,
        })?;
    let (updated, current) = bump_workspace_version(&manifest, requested)?;
    if requested <= current {
        return Err(ReleaseError::NotGreater { requested, current });
    }
    std::fs::write(&manifest_path, updated).map_err(|source| ReleaseError::Write {
        path: manifest_path.clone(),
        source,
    })?;

    // Every member inherits `version.workspace = true`; refresh their lock
    // entries without touching dependency resolution.
    cargo(&root, &["update", "--workspace", "--offline"])?;

    if args.no_commit {
        println!("bumped {current} -> {requested} (not committed)");
        return Ok(());
    }

    let message = format!("chore(release): v{requested}");
    git(
        &root,
        &["add", "Cargo.toml", "Cargo.lock"],
        Stdio::inherit(),
    )?
    .success_or("git", "add")?;
    git(&root, &["commit", "-q", "-m", &message], Stdio::inherit())?.success_or("git", "commit")?;
    let tag = format!("v{requested}");
    git(
        &root,
        &["tag", "-a", &tag, "-m", &message],
        Stdio::inherit(),
    )?
    .success_or("git", "tag")?;

    println!("released {current} -> {requested}");
    println!("  commit: {message}");
    println!("  tag:    {tag}");
    println!("push with: git push origin main --follow-tags");
    Ok(())
}

/// Rewrites the `version` line inside `[workspace.package]` and returns the
/// new manifest with the version it replaced.
pub fn bump_workspace_version(
    manifest: &str,
    requested: Version,
) -> Result<(String, Version), ReleaseError> {
    let mut out = String::with_capacity(manifest.len());
    let mut in_workspace_package = false;
    let mut replaced = None;
    for line in manifest.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace_package = trimmed == "[workspace.package]";
        }
        if in_workspace_package
            && replaced.is_none()
            && let Some(rest) = trimmed.strip_prefix("version")
            && let Some(rest) = rest.trim_start().strip_prefix('=')
            && let Some(quoted) = rest.trim().strip_prefix('"')
            && let Some(end) = quoted.find('"')
        {
            let current = &quoted[..end];
            let Some(current) = Version::parse(current) else {
                return Err(ReleaseError::InvalidVersion(current.to_owned()));
            };
            let line_ending = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            out.push_str(&format!("version = \"{requested}\"{line_ending}"));
            replaced = Some(current);
            continue;
        }
        out.push_str(line);
    }
    match replaced {
        Some(current) => Ok((out, current)),
        None => Err(ReleaseError::VersionLineMissing),
    }
}

fn git(root: &Path, args: &[&str], stderr: Stdio) -> Result<ExitStatus, ReleaseError> {
    ProcessCommand::new("git")
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(stderr)
        .status()
        .map_err(|source| ReleaseError::Launch {
            program: "git",
            source,
        })
}

fn cargo(root: &Path, args: &[&str]) -> Result<(), ReleaseError> {
    let program = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = ProcessCommand::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|source| ReleaseError::Launch {
            program: "cargo",
            source,
        })?;
    status.success_or("cargo", &args.join(" "))
}

trait SuccessOr {
    fn success_or(self, program: &'static str, args: &str) -> Result<(), ReleaseError>;
}

impl SuccessOr for ExitStatus {
    fn success_or(self, program: &'static str, args: &str) -> Result<(), ReleaseError> {
        if self.success() {
            Ok(())
        } else {
            Err(ReleaseError::Failed {
                program,
                args: args.to_owned(),
                status: self,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_semver_only() {
        assert_eq!(
            Version::parse("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        for rejected in ["1.2", "1.2.3.4", "v1.2.3", "1.2.3-rc1", "1.2.x", ""] {
            assert_eq!(Version::parse(rejected), None, "{rejected:?}");
        }
        assert!(Version::parse("0.2.0") > Version::parse("0.1.9"));
        assert!(Version::parse("1.0.0") > Version::parse("0.99.99"));
    }

    #[test]
    fn bumps_only_the_workspace_package_version() {
        let manifest = "[package]\nname = \"qq\"\nversion.workspace = true\n\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace.dependencies]\nserde = { version = \"1\" }\ntokio = \"1.0.0\"\n";
        let (updated, current) =
            bump_workspace_version(manifest, Version::parse("0.2.0").unwrap()).unwrap();
        assert_eq!(current, Version::parse("0.1.0").unwrap());
        assert_eq!(
            updated,
            "[package]\nname = \"qq\"\nversion.workspace = true\n\n[workspace.package]\nversion = \"0.2.0\"\nedition = \"2024\"\n\n[workspace.dependencies]\nserde = { version = \"1\" }\ntokio = \"1.0.0\"\n"
        );
    }

    #[test]
    fn rejects_a_manifest_without_a_workspace_version() {
        let manifest = "[package]\nname = \"qq\"\nversion = \"0.1.0\"\n";
        let error = bump_workspace_version(manifest, Version::parse("0.2.0").unwrap()).unwrap_err();
        assert!(matches!(error, ReleaseError::VersionLineMissing), "{error}");
    }

    #[test]
    fn real_manifest_round_trips() {
        let manifest = include_str!("../../Cargo.toml");
        let (updated, current) =
            bump_workspace_version(manifest, Version::parse("999.0.0").unwrap()).unwrap();
        assert_eq!(current.to_string(), env!("CARGO_PKG_VERSION"));
        assert_eq!(updated.matches("999.0.0").count(), 1);
        assert_eq!(updated.lines().count(), manifest.lines().count());
    }
}
