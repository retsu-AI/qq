//! `cargo xtask release`: bump the workspace version, then tag the merged
//! result.
//!
//! `main` only accepts pull requests and merges rewrite commit SHAs, so a
//! release is two steps: `cargo xtask release X.Y.Z` commits the bump and a
//! new `CHANGELOG.md` section on the current branch for a PR, and `cargo
//! xtask release --tag` on the merged `main` creates `vX.Y.Z` from the
//! manifest. The release workflow refuses a tag whose version differs from
//! the manifest or whose commit is not on `main`. Nothing is pushed here.

use std::{
    env, fmt, io,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitStatus, Stdio},
};

use clap::Args;
use thiserror::Error;

mod changelog;

#[derive(Debug, Args)]
pub struct ReleaseArgs {
    /// Version to release, e.g. `0.2.0`. Must be greater than the current
    /// workspace version. Bumps, writes the `CHANGELOG.md` section from the
    /// Conventional Commit subjects since the last tag, and commits on the
    /// current branch.
    #[arg(required_unless_present = "tag", conflicts_with = "tag")]
    version: Option<String>,
    /// Update `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md` but do not commit.
    #[arg(long, conflicts_with = "tag")]
    no_commit: bool,
    /// Tag the checked-out `main` with the manifest version. Run after the
    /// bump PR has merged and `main` is pulled.
    #[arg(long)]
    tag: bool,
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
    #[error(
        "--tag must run on main at origin/main (HEAD is {head}, origin/main is {origin}); pull first"
    )]
    NotAtOriginMain { head: String, origin: String },
    #[error("cannot resolve {0}; fetch origin first")]
    Unresolvable(&'static str),
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
    let root = env::current_dir().map_err(|source| ReleaseError::Read {
        path: PathBuf::from("."),
        source,
    })?;
    let manifest_path = root.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Err(ReleaseError::NotRepositoryRoot(root));
    }
    let manifest =
        std::fs::read_to_string(&manifest_path).map_err(|source| ReleaseError::Read {
            path: manifest_path.clone(),
            source,
        })?;

    if args.tag {
        return tag_main(&root, &manifest);
    }

    let requested_text = args.version.as_deref().unwrap_or_default();
    let requested = Version::parse(requested_text)
        .ok_or_else(|| ReleaseError::InvalidVersion(requested_text.to_owned()))?;

    if !args.no_commit {
        let status = git(&root, &["diff", "--quiet", "HEAD", "--"], Stdio::inherit())?;
        if !status.success() {
            return Err(ReleaseError::DirtyWorktree);
        }
    }

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

    // Install pins and `--version` samples in the guide follow the release.
    let mut rewritten_docs = Vec::new();
    for path in versioned_doc_paths(&root)? {
        let text = std::fs::read_to_string(&path).map_err(|source| ReleaseError::Read {
            path: path.clone(),
            source,
        })?;
        let (updated, count) = rewrite_version_tokens(&text, current, requested);
        if count == 0 {
            continue;
        }
        std::fs::write(&path, updated).map_err(|source| ReleaseError::Write {
            path: path.clone(),
            source,
        })?;
        let relative = path.strip_prefix(&root).unwrap_or(&path).to_owned();
        rewritten_docs.push((relative, count));
    }

    // The changelog is read from git before anything is committed, so the
    // bump commit itself is never listed.
    let subjects = changelog::subjects_since_last_tag(&root)?;
    let section = changelog::render_section(requested, &changelog::today_utc(), &subjects);
    let changelog_path = root.join(changelog::FILE_NAME);
    let existing = match std::fs::read_to_string(&changelog_path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(ReleaseError::Read {
                path: changelog_path,
                source,
            });
        }
    };
    std::fs::write(
        &changelog_path,
        changelog::prepend_section(existing.as_deref(), &section),
    )
    .map_err(|source| ReleaseError::Write {
        path: changelog_path.clone(),
        source,
    })?;

    if args.no_commit {
        println!("bumped {current} -> {requested} (not committed)");
        println!("  {} entries in {}", subjects.len(), changelog::FILE_NAME);
        for (path, count) in &rewritten_docs {
            println!("  {}: {count} version reference(s)", path.display());
        }
        return Ok(());
    }

    let message = format!("chore(release): v{requested}");
    let mut staged: Vec<String> = ["Cargo.toml", "Cargo.lock", changelog::FILE_NAME]
        .map(str::to_owned)
        .to_vec();
    staged.extend(
        rewritten_docs
            .iter()
            .map(|(path, _)| path.to_string_lossy().into_owned()),
    );
    let mut add = vec!["add", "--"];
    add.extend(staged.iter().map(String::as_str));
    git(&root, &add, Stdio::inherit())?.success_or("git", "add")?;
    git(&root, &["commit", "-q", "-m", &message], Stdio::inherit())?.success_or("git", "commit")?;

    println!("bumped {current} -> {requested}");
    println!("  commit: {message}");
    println!(
        "  {}: {} entries under \"## {requested}\"",
        changelog::FILE_NAME,
        subjects.len()
    );
    for (path, count) in &rewritten_docs {
        println!("  {}: {count} version reference(s)", path.display());
    }
    println!("next: push this branch, open a PR titled \"{message}\", merge it, then");
    println!("      git switch main && git pull --ff-only && cargo xtask release --tag");
    Ok(())
}

/// Files whose QQ version strings follow the release: the user guide and the
/// README. The root crate's docs-truth test fails when any of them names a
/// version other than the manifest's, so the bump must carry them along.
const VERSIONED_DOCS: [&str; 2] = ["docs/guide", "README.md"];

/// Marks a line whose version strings are not QQ's (an upstream client, an
/// example pack): the release leaves the line alone and docs-truth does not
/// hold it to the workspace version. Shared with `src/docs_truth.rs`.
pub const NOT_QQ_VERSION: &str = "<!-- not-qq-version -->";

/// The tracked Markdown files under [`VERSIONED_DOCS`], sorted. Only what
/// `git ls-files` reports is rewritten, and only regular files: an
/// untracked draft or a symlink is never read, written, or committed.
fn versioned_doc_paths(root: &Path) -> Result<Vec<PathBuf>, ReleaseError> {
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(VERSIONED_DOCS);
    let output = ProcessCommand::new("git")
        .args(&args)
        .current_dir(root)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|source| ReleaseError::Launch {
            program: "git",
            source,
        })?;
    if !output.status.success() {
        return Err(ReleaseError::Failed {
            program: "git",
            args: args.join(" "),
            status: output.status,
        });
    }
    let mut paths = Vec::new();
    for name in output.stdout.split(|byte| *byte == 0) {
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        if !name.ends_with(".md") {
            continue;
        }
        let path = root.join(name);
        let metadata = std::fs::symlink_metadata(&path).map_err(|source| ReleaseError::Read {
            path: path.clone(),
            source,
        })?;
        if metadata.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Replaces every whole `current` version token in `text` with `requested`
/// and returns the new text with the number replaced. A token is whole when
/// it is not part of a longer dotted number: `0.1.4` matches in `v0.1.4` and
/// `qq 0.1.4 (`, never inside `10.1.4` or `0.1.4.1`. Lines carrying
/// [`NOT_QQ_VERSION`] are left unchanged.
pub fn rewrite_version_tokens(text: &str, current: Version, requested: Version) -> (String, usize) {
    let current = current.to_string();
    let requested = requested.to_string();
    let mut out = String::with_capacity(text.len());
    let mut replaced = 0;
    for line in text.split_inclusive('\n') {
        if line.contains(NOT_QQ_VERSION) {
            out.push_str(line);
            continue;
        }
        let bytes = line.as_bytes();
        let mut copied = 0;
        for (at, _) in line.match_indices(&current) {
            let end = at + current.len();
            let joined_before = at > 0 && (bytes[at - 1].is_ascii_digit() || bytes[at - 1] == b'.');
            let joined_after = end < bytes.len()
                && (bytes[end].is_ascii_digit()
                    || (bytes[end] == b'.' && bytes.get(end + 1).is_some_and(u8::is_ascii_digit)));
            if joined_before || joined_after {
                continue;
            }
            out.push_str(&line[copied..at]);
            out.push_str(&requested);
            copied = end;
            replaced += 1;
        }
        out.push_str(&line[copied..]);
    }
    (out, replaced)
}

/// Creates `vX.Y.Z` from the manifest version on a clean `main` that matches
/// `origin/main`, so the tag always names a commit the release workflow will
/// find on the default branch.
fn tag_main(root: &Path, manifest: &str) -> Result<(), ReleaseError> {
    let status = git(root, &["diff", "--quiet", "HEAD", "--"], Stdio::inherit())?;
    if !status.success() {
        return Err(ReleaseError::DirtyWorktree);
    }
    let head =
        git_output(root, &["rev-parse", "HEAD"]).ok_or(ReleaseError::Unresolvable("HEAD"))?;
    let origin = git_output(root, &["rev-parse", "origin/main"])
        .ok_or(ReleaseError::Unresolvable("origin/main"))?;
    if head != origin {
        return Err(ReleaseError::NotAtOriginMain {
            head: head[..7.min(head.len())].to_owned(),
            origin: origin[..7.min(origin.len())].to_owned(),
        });
    }

    let (_, current) = bump_workspace_version(manifest, Version::parse("0.0.0").unwrap())?;
    let tag = format!("v{current}");
    let status = git(
        root,
        &["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")],
        Stdio::null(),
    )?;
    if status.success() {
        return Err(ReleaseError::TagExists(current));
    }
    let message = format!("chore(release): {tag}");
    git(root, &["tag", "-a", &tag, "-m", &message], Stdio::inherit())?.success_or("git", "tag")?;

    println!("tagged {tag} at {}", &head[..7.min(head.len())]);
    println!("push with: git push origin {tag}");
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = ProcessCommand::new("git")
        .args(args)
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
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
    fn rewrites_whole_version_tokens_only() {
        let current = Version::parse("0.1.4").unwrap();
        let requested = Version::parse("0.2.0").unwrap();
        let text = "--version 0.1.4 --dir x\nnix run github:o/qq/v0.1.4\n`qq 0.1.4 (abc 2026-09-22)`\n\
                    10.1.4 and 0.1.40 and 0.1.4.1 and client 0.156.1 stay\nends 0.1.4.\n";
        let (updated, count) = rewrite_version_tokens(text, current, requested);
        assert_eq!(count, 4, "{updated}");
        assert_eq!(
            updated,
            "--version 0.2.0 --dir x\nnix run github:o/qq/v0.2.0\n`qq 0.2.0 (abc 2026-09-22)`\n\
             10.1.4 and 0.1.40 and 0.1.4.1 and client 0.156.1 stay\nends 0.2.0.\n"
        );
        let (unchanged, none) = rewrite_version_tokens("no versions here", current, requested);
        assert_eq!((unchanged.as_str(), none), ("no versions here", 0));

        // A marked line keeps a foreign version even when it equals QQ's.
        let marked = format!("pack version: \"0.1.4\", {NOT_QQ_VERSION}\nqq 0.1.4\n");
        let (updated, count) = rewrite_version_tokens(&marked, current, requested);
        assert_eq!(count, 1);
        assert_eq!(
            updated,
            format!("pack version: \"0.1.4\", {NOT_QQ_VERSION}\nqq 0.2.0\n")
        );
    }

    #[test]
    fn not_qq_version_marker_matches_the_docs_truth_test() {
        let docs_truth = include_str!("../../src/docs_truth.rs");
        assert!(
            docs_truth.contains(&format!(
                "const NOT_QQ_VERSION: &str = \"{NOT_QQ_VERSION}\";"
            )),
            "src/docs_truth.rs must use the same NOT_QQ_VERSION marker"
        );
    }

    #[test]
    fn versioned_docs_skip_untracked_files_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let git = |args: &[&str]| {
            let status = ProcessCommand::new("git")
                .args(args)
                .current_dir(root)
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::create_dir_all(root.join("docs/guide")).unwrap();
        std::fs::write(root.join("docs/guide/install.md"), "0.1.4").unwrap();
        std::fs::write(root.join("README.md"), "0.1.4").unwrap();
        git(&["add", "docs/guide/install.md", "README.md"]);
        std::fs::write(root.join("docs/guide/draft.md"), "0.1.4").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("README.md"), root.join("docs/guide/link.md"))
            .unwrap();
        #[cfg(unix)]
        git(&["add", "docs/guide/link.md"]);
        let paths = versioned_doc_paths(root).unwrap();
        assert_eq!(
            paths,
            [root.join("README.md"), root.join("docs/guide/install.md")]
        );
    }

    #[test]
    fn versioned_docs_cover_the_guide_and_readme() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let paths = versioned_doc_paths(&root).unwrap();
        assert!(paths.iter().any(|path| path.ends_with("README.md")));
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("docs/guide/install.md"))
        );
        assert!(paths.iter().all(|path| path.is_file()), "{paths:?}");
    }

    /// `main` carries the version of the last release until the next bump PR.
    /// A branch cut before a release and merged after it can quietly carry the
    /// old manifest back onto `main` (this happened in #46: 0.1.0 → 0.0.0), so
    /// the workspace version must never be below the newest `v*` tag.
    #[test]
    fn workspace_version_is_not_behind_the_newest_release_tag() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let Some(tags) = git_output(&root, &["tag", "--list", "v*"]) else {
            // A shallow or tagless checkout has nothing to compare against.
            return;
        };
        let newest = tags
            .lines()
            .filter_map(|tag| Version::parse(tag.trim().strip_prefix('v')?))
            .max();
        let Some(newest) = newest else {
            return;
        };
        let manifest = include_str!("../../Cargo.toml");
        let (_, current) =
            bump_workspace_version(manifest, Version::parse("0.0.0").unwrap()).unwrap();
        assert!(
            current >= newest,
            "Cargo.toml says {current} but v{newest} is released; a stale branch reverted the bump"
        );
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
