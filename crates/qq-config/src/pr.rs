//! Rules for `qq pr status`: which reviewers must finish with the head commit
//! and which CI checks must pass. Loaded from `pr.ron`, global then project,
//! like `tui.ron`; each later source replaces the lists it sets.
//!
//! The file names reviewer accounts and check names only. It grants no
//! execution or approval authority, so it needs no workspace trust.
use std::path::Path;

use ron::{Options, extensions::Extensions};
use serde::Deserialize;

use super::{
    ConfigError, ConfigLoader, SourceIdentity, SourceKind,
    loader::{
        Probes, canonical_working_directory, discover_file, project_directories, read_candidate,
    },
};

/// Most reviewer rules one effective configuration may name.
pub const MAX_PR_REVIEWERS: usize = 16;
/// Most rule entries in one reviewer, and most required check names.
pub const MAX_PR_RULE_ITEMS: usize = 16;
const MAX_PR_NAME_BYTES: usize = 256;
const MAX_START_WITHIN_SECONDS: u64 = 86_400;

/// When a reviewer counts as finished with the head commit. Any one suffices.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrReviewerDone {
    /// The account submitted a review whose commit is the head.
    Review,
    /// A check run or commit status with this name completed on the head.
    Check(String),
    /// A comment by the account contains this text after `{sha}` and
    /// `{sha10}` are replaced with the head commit.
    Comment(String),
}

/// One reviewer whose finished review is required before a PR is ready.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrReviewer {
    /// Forge account login, without a `[bot]` suffix.
    pub login: String,
    pub done_when: Vec<PrReviewerDone>,
    /// Seconds after the head commit before a reviewer that has not started
    /// is reported as an action, not as waiting.
    #[serde(default)]
    pub start_within_seconds: Option<u64>,
    /// Text the agent can post to request a review; only shown, never posted.
    #[serde(default)]
    pub request: Option<String>,
    /// The reviewer ignores draft PRs, so a draft is reported as an action.
    #[serde(default)]
    pub skips_drafts: bool,
}

/// Effective `qq pr status` rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrStatusConfig {
    required_checks: Vec<String>,
    reviewers: Vec<PrReviewer>,
    sources: Vec<SourceIdentity>,
}

impl PrStatusConfig {
    /// Check names that must pass in addition to the forge's required checks.
    #[must_use]
    pub fn required_checks(&self) -> &[String] {
        &self.required_checks
    }

    #[must_use]
    pub fn reviewers(&self) -> &[PrReviewer] {
        &self.reviewers
    }

    /// Files that contributed, in precedence order.
    #[must_use]
    pub fn sources(&self) -> &[SourceIdentity] {
        &self.sources
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    #[serde(default)]
    required_checks: Option<Vec<String>>,
    #[serde(default)]
    reviewers: Option<Vec<PrReviewer>>,
}

fn invalid(origin: &SourceIdentity, message: impl Into<String>) -> ConfigError {
    ConfigError::Parse {
        origin: origin.clone(),
        message: message.into(),
    }
}

fn valid_name(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_PR_NAME_BYTES
}

impl Document {
    fn parse(content: &str, origin: &SourceIdentity) -> Result<Self, ConfigError> {
        let options = Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
        let document: Self = options
            .from_str(content)
            .map_err(|error| invalid(origin, error.to_string()))?;
        if document.version != 1 {
            return Err(ConfigError::UnsupportedVersion {
                origin: origin.clone(),
                version: document.version,
            });
        }
        if let Some(checks) = &document.required_checks {
            if checks.len() > MAX_PR_RULE_ITEMS {
                return Err(invalid(
                    origin,
                    format!("required_checks has more than {MAX_PR_RULE_ITEMS} names"),
                ));
            }
            if !checks.iter().all(|name| valid_name(name)) {
                return Err(invalid(
                    origin,
                    "required_checks names must be non-empty and at most 256 bytes",
                ));
            }
        }
        if let Some(reviewers) = &document.reviewers {
            if reviewers.len() > MAX_PR_REVIEWERS {
                return Err(invalid(
                    origin,
                    format!("reviewers has more than {MAX_PR_REVIEWERS} entries"),
                ));
            }
            for reviewer in reviewers {
                validate_reviewer(reviewer, origin)?;
            }
        }
        Ok(document)
    }
}

fn validate_reviewer(reviewer: &PrReviewer, origin: &SourceIdentity) -> Result<(), ConfigError> {
    let login = &reviewer.login;
    if !valid_name(login) || login.ends_with("[bot]") {
        return Err(invalid(
            origin,
            format!(
                "reviewer login `{login}` must be non-empty, at most 256 bytes and without the `[bot]` suffix"
            ),
        ));
    }
    if reviewer.done_when.is_empty() || reviewer.done_when.len() > MAX_PR_RULE_ITEMS {
        return Err(invalid(
            origin,
            format!("reviewer `{login}` needs 1 to {MAX_PR_RULE_ITEMS} done_when rules"),
        ));
    }
    for rule in &reviewer.done_when {
        match rule {
            PrReviewerDone::Review => {}
            PrReviewerDone::Check(text) | PrReviewerDone::Comment(text) => {
                if !valid_name(text) {
                    return Err(invalid(
                        origin,
                        format!("reviewer `{login}` has an empty or oversized done_when pattern"),
                    ));
                }
            }
        }
    }
    if reviewer
        .start_within_seconds
        .is_some_and(|seconds| seconds == 0 || seconds > MAX_START_WITHIN_SECONDS)
    {
        return Err(invalid(
            origin,
            format!("reviewer `{login}` start_within_seconds must be 1 to 86400"),
        ));
    }
    if reviewer
        .request
        .as_deref()
        .is_some_and(|request| !valid_name(request))
    {
        return Err(invalid(
            origin,
            format!("reviewer `{login}` request must be non-empty and at most 256 bytes"),
        ));
    }
    Ok(())
}

pub(super) fn load(loader: &ConfigLoader, cwd: &Path) -> Result<PrStatusConfig, ConfigError> {
    let cwd = canonical_working_directory(cwd)?;
    let mut probes = Probes::default();
    let mut candidates = Vec::new();
    if let Some(global) = discover_file(
        loader.paths.global_dir.join("pr.ron"),
        SourceKind::Global,
        false,
        &mut probes,
    )? {
        candidates.push(global);
    }
    for directory in project_directories(&cwd, &mut probes) {
        if let Some(project) = discover_file(
            directory.join(".qq/pr.ron"),
            SourceKind::Project,
            false,
            &mut probes,
        )? {
            candidates.push(project);
        }
    }
    let mut config = PrStatusConfig::default();
    for candidate in candidates {
        let (source, content) = read_candidate(&candidate)?;
        let document = Document::parse(&content, &source)?;
        if let Some(checks) = document.required_checks {
            config.required_checks = checks;
        }
        if let Some(reviewers) = document.reviewers {
            config.reviewers = reviewers;
        }
        config.sources.push(source);
    }
    Ok(config)
}
