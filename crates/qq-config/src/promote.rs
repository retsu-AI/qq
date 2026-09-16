//! Durable promotion of approval grants into workspace configuration.
//!
//! The approval prompt's "always allow in this workspace" choice lands here:
//! a grant is appended to the `policy` section of the workspace's
//! `.qq/config.ron` by targeted text insertion, so unrelated user content and
//! formatting survive untouched. The updated document must reparse and
//! declare the grant before anything is written, and the write itself is
//! atomic (temp file + rename).

use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

use sha2::{Digest, Sha256};

use super::{
    ConfigError, ConfigLoader, GrantPromotion, PromotionOutcome, SourceIdentity, SourceKind,
    WorkspaceGrant, document, document::Document, loader,
};

pub(super) fn promote_workspace_grant(
    config_loader: &ConfigLoader,
    workspace_dir: &Path,
    grant: &WorkspaceGrant,
) -> Result<GrantPromotion, ConfigError> {
    validate_grant(grant)?;
    refuse_if_managed_denies(config_loader, grant)?;
    let _workspace_lock = WorkspaceGrantLock::acquire(config_loader, workspace_dir)?;

    let directory = workspace_dir.join(".qq");
    let path = directory.join("config.ron");
    let existing = loader::discover_file(
        path.clone(),
        SourceKind::Project,
        false,
        &mut loader::Probes::default(),
    )?;
    let (content, prior_sensitive_digest, path) = match existing {
        None => (render_new_document(grant), None, path),
        Some(candidate) => {
            let canonical = candidate.path.clone();
            let (source, content) = loader::read_candidate(&candidate)?;
            let current = Document::parse(&content, &source)?;
            if current.declares_policy_grant(grant) {
                return Ok(GrantPromotion::new(
                    canonical,
                    PromotionOutcome::AlreadyPresent,
                ));
            }
            let updated = insert_grant(&content, grant).ok_or_else(|| ConfigError::Parse {
                origin: source.clone(),
                message: "could not locate an insertion point for the workspace grant; add it \
                          to the policy section manually"
                    .to_owned(),
            })?;
            (updated, current.sensitive_digest()?, canonical)
        }
    };

    // The result must round-trip before anything touches the disk.
    let source = SourceIdentity::file(SourceKind::Project, path.clone());
    let document = Document::parse(&content, &source)?;
    if !document.declares_policy_grant(grant) {
        return Err(ConfigError::Parse {
            origin: source,
            message: "the promoted workspace grant did not survive reparsing".to_owned(),
        });
    }

    fs::create_dir_all(&directory).map_err(|error| ConfigError::Io {
        path: directory.clone(),
        error,
    })?;
    loader::atomic_write(&path, content.as_bytes())?;
    record_trust(config_loader, &path, &document, prior_sensitive_digest)?;
    Ok(GrantPromotion::new(path, PromotionOutcome::Added))
}

/// Serializes the complete workspace-config read-modify-write across loader
/// instances and processes. The lock lives in QQ's private data directory so
/// granting a capability never leaves an untracked lock file in the project.
struct WorkspaceGrantLock(File);

impl WorkspaceGrantLock {
    fn acquire(config_loader: &ConfigLoader, workspace_dir: &Path) -> Result<Self, ConfigError> {
        let data_dir = config_loader.paths().data_dir();
        loader::ensure_data_directory(data_dir)?;
        let canonical = fs::canonicalize(workspace_dir).map_err(|error| ConfigError::Io {
            path: workspace_dir.to_owned(),
            error,
        })?;
        let digest = Sha256::digest(canonical.as_os_str().to_string_lossy().as_bytes());
        let path = data_dir.join(format!("workspace-grant-{digest:x}.lock"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ConfigError::SymlinkSource { path });
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(ConfigError::NotRegularFile { path });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ConfigError::Io { path, error }),
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|error| ConfigError::Io {
            path: path.clone(),
            error,
        })?;
        #[cfg(unix)]
        loader::validate_private_file_mode(
            &path,
            &file.metadata().map_err(|error| ConfigError::Io {
                path: path.clone(),
                error,
            })?,
        )?;
        file.lock().map_err(|error| ConfigError::Io {
            path: path.clone(),
            error,
        })?;
        Ok(Self(file))
    }
}

impl Drop for WorkspaceGrantLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Trusts the file's new sensitive digest when the promotion itself is the
/// only untrusted change: the write is an explicit user decision, so the very
/// next load must not demand re-trusting the file the approval prompt just
/// wrote. A file whose prior sensitive operations were still pending trust
/// stays pending — promotion must not launder trust for content the user
/// never reviewed.
fn record_trust(
    config_loader: &ConfigLoader,
    path: &Path,
    document: &Document,
    prior_sensitive_digest: Option<String>,
) -> Result<(), ConfigError> {
    let Some(digest) = document.sensitive_digest()? else {
        return Ok(());
    };
    let canonical = fs::canonicalize(path).map_err(|error| ConfigError::Io {
        path: path.to_owned(),
        error,
    })?;
    let _lock = loader::TrustStateLock::acquire(config_loader.paths())?;
    let mut trust =
        loader::TrustState::load(config_loader.paths(), &mut loader::Probes::default())?;
    let prior_trusted = match prior_sensitive_digest {
        None => true,
        Some(prior) => trust.contains(&canonical, &prior),
    };
    if prior_trusted {
        trust.insert(canonical, digest);
        trust.save(config_loader.paths())?;
    }
    Ok(())
}

fn validate_grant(grant: &WorkspaceGrant) -> Result<(), ConfigError> {
    match grant {
        WorkspaceGrant::Tool(name) => document::validate_tool_grant_name(name),
        WorkspaceGrant::ShellPrefix(prefix) => document::validate_shell_prefix_grant(prefix),
        WorkspaceGrant::Host(host) => document::validate_host_grant(host),
    }
    .map_err(|message| ConfigError::InvalidGrant { message })
}

/// Scans the managed layer (administrator files plus MDM) for deny lists and
/// refuses to persist a grant they filter out; writing it would only produce
/// a grant the effective set immediately drops.
fn refuse_if_managed_denies(
    config_loader: &ConfigLoader,
    grant: &WorkspaceGrant,
) -> Result<(), ConfigError> {
    let mut deny_tools = Vec::new();
    let mut deny_shell_prefixes = Vec::new();
    let mut deny_hosts = Vec::new();
    let paths = config_loader.paths();
    if paths.enforce_managed_ownership {
        loader::validate_managed_directory_if_present(paths.managed_dir())?;
    }
    for candidate in loader::discover_layer_directory(
        paths.managed_dir(),
        "managed.ron",
        "managed.d",
        SourceKind::Managed,
        &mut loader::Probes::default(),
    )? {
        if paths.enforce_managed_ownership {
            loader::validate_managed_file(&candidate.path)?;
        }
        let (source, content) = loader::read_candidate(&candidate)?;
        let document = Document::parse(&content, &source)?;
        document.collect_policy_denies(&mut deny_tools, &mut deny_shell_prefixes, &mut deny_hosts);
    }
    if let Some(mdm) = loader::read_mdm_document(config_loader)? {
        mdm.document.collect_policy_denies(
            &mut deny_tools,
            &mut deny_shell_prefixes,
            &mut deny_hosts,
        );
    }
    match grant {
        WorkspaceGrant::Tool(name) => {
            if deny_tools.iter().any(|denied| denied == name) {
                return Err(ConfigError::GrantDeniedByManaged {
                    grant: name.clone(),
                    rule: "deny_tools",
                });
            }
        }
        WorkspaceGrant::ShellPrefix(prefix) => {
            if deny_shell_prefixes
                .iter()
                .any(|denied| document::shell_prefixes_overlap(denied, prefix))
            {
                return Err(ConfigError::GrantDeniedByManaged {
                    grant: prefix.clone(),
                    rule: "deny_shell_prefixes",
                });
            }
        }
        WorkspaceGrant::Host(host) => {
            if deny_hosts
                .iter()
                .any(|denied| document::hosts_overlap(denied, host))
            {
                return Err(ConfigError::GrantDeniedByManaged {
                    grant: host.clone(),
                    rule: "deny_hosts",
                });
            }
        }
    }
    Ok(())
}

const fn grant_field(grant: &WorkspaceGrant) -> &'static str {
    match grant {
        WorkspaceGrant::Tool(_) => "allow_tools",
        WorkspaceGrant::ShellPrefix(_) => "allow_shell_prefixes",
        WorkspaceGrant::Host(_) => "allow_hosts",
    }
}

fn ron_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn render_new_document(grant: &WorkspaceGrant) -> String {
    format!(
        "(\n    version: 1,\n    policy: (\n        {field}: [\n            {literal},\n        \
         ],\n    ),\n)\n",
        field = grant_field(grant),
        literal = ron_string(grant.value()),
    )
}

/// Byte offsets of the structural anchors a grant insertion needs, located by
/// a comment- and string-aware scan rather than a reserialization, so user
/// formatting and comments survive.
#[derive(Default)]
struct Anchors {
    /// Just after the top-level struct's `(`.
    root_open: Option<usize>,
    /// Just after the `policy` group's `(`.
    policy_open: Option<usize>,
    /// Just after the target grant list's `[`.
    list_open: Option<usize>,
    /// At the target grant list's `]`.
    list_close: Option<usize>,
}

/// Inserts the grant with targeted text edits: append to the existing list,
/// else add the list to the existing policy group, else add a policy section
/// to the document. Returns None when the scan cannot place the edit safely
/// (for example around raw strings); the caller reports that instead of
/// risking a corrupting write.
fn insert_grant(content: &str, grant: &WorkspaceGrant) -> Option<String> {
    let anchors = locate_anchors(content, grant_field(grant))?;
    let literal = ron_string(grant.value());
    let field = grant_field(grant);
    let (offset, insertion) = if let Some(open) = anchors.list_open {
        let close = anchors.list_close?;
        let inner = content.get(open..close)?;
        let insertion = if inner.trim().is_empty() {
            literal
        } else if let Some(indent) = multiline_item_indent(inner) {
            format!("\n{indent}{literal},")
        } else {
            format!("{literal}, ")
        };
        (open, insertion)
    } else if let Some(open) = anchors.policy_open {
        (open, format!("{field}: [{literal}], "))
    } else {
        let open = anchors.root_open?;
        (
            open,
            format!(
                "\n    policy: (\n        {field}: [\n            {literal},\n        ],\n    ),"
            ),
        )
    };
    let mut updated = String::with_capacity(content.len() + insertion.len());
    updated.push_str(content.get(..offset)?);
    updated.push_str(&insertion);
    updated.push_str(content.get(offset..)?);
    Some(updated)
}

/// For a list already broken across lines, the indentation of its first item
/// so the inserted grant matches the surrounding style.
fn multiline_item_indent(inner: &str) -> Option<String> {
    let mut lines = inner.split('\n');
    if !lines.next()?.trim().is_empty() {
        return None;
    }
    lines
        .find(|line| !line.trim().is_empty())
        .map(|line| line[..line.len() - line.trim_start().len()].to_owned())
}

fn locate_anchors(content: &str, field: &str) -> Option<Anchors> {
    enum State {
        Code,
        Str,
        LineComment,
        BlockComment(u32),
    }

    let bytes = content.as_bytes();
    let mut anchors = Anchors::default();
    let mut state = State::Code;
    let mut stack: Vec<u8> = Vec::new();
    // Span of the identifier currently or last being read at this nesting.
    let mut ident: Option<(usize, usize)> = None;
    // A field name that saw its `:` and is waiting for its value.
    let mut pending: Option<String> = None;
    let mut policy_depth: Option<usize> = None;
    let mut list_depth: Option<usize> = None;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            State::Str => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    state = State::Code;
                }
            }
            State::LineComment => {
                if byte == b'\n' {
                    state = State::Code;
                }
            }
            State::BlockComment(depth) => {
                if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
                    state = State::BlockComment(depth + 1);
                    index += 1;
                } else if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    index += 1;
                    state = if depth == 1 {
                        State::Code
                    } else {
                        State::BlockComment(depth - 1)
                    };
                }
            }
            State::Code => match byte {
                b'"' => {
                    // A raw string would defeat this scan; give up rather
                    // than risk corrupting the document.
                    if index > 0 && matches!(bytes[index - 1], b'r' | b'#') {
                        return None;
                    }
                    state = State::Str;
                    ident = None;
                    pending = None;
                }
                b'/' if bytes.get(index + 1) == Some(&b'/') => {
                    state = State::LineComment;
                    index += 1;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = State::BlockComment(1);
                    index += 1;
                }
                b'(' | b'[' | b'{' => {
                    let matched = pending.take();
                    if stack.is_empty() && byte == b'(' && anchors.root_open.is_none() {
                        anchors.root_open = Some(index + 1);
                    } else if byte == b'('
                        && stack.len() == 1
                        && anchors.policy_open.is_none()
                        && matched.as_deref() == Some("policy")
                    {
                        anchors.policy_open = Some(index + 1);
                        policy_depth = Some(stack.len() + 1);
                    } else if byte == b'['
                        && policy_depth == Some(stack.len())
                        && anchors.list_open.is_none()
                        && matched.as_deref() == Some(field)
                    {
                        anchors.list_open = Some(index + 1);
                        list_depth = Some(stack.len() + 1);
                    }
                    stack.push(byte);
                    ident = None;
                }
                b')' | b']' | b'}' => {
                    if byte == b']' && list_depth == Some(stack.len()) {
                        anchors.list_close = Some(index);
                        list_depth = None;
                    }
                    if byte == b')' && policy_depth == Some(stack.len()) {
                        policy_depth = None;
                    }
                    stack.pop()?;
                    ident = None;
                    pending = None;
                }
                b':' => {
                    pending = ident
                        .take()
                        .and_then(|(start, end)| content.get(start..end))
                        .map(str::to_owned);
                }
                b',' => {
                    ident = None;
                    pending = None;
                }
                _ if byte.is_ascii_whitespace() => {}
                _ if byte.is_ascii_alphanumeric() || byte == b'_' => {
                    ident = match ident {
                        Some((start, end)) if end == index => Some((start, index + 1)),
                        _ => Some((index, index + 1)),
                    };
                }
                _ => {
                    ident = None;
                    pending = None;
                }
            },
        }
        index += 1;
    }
    // An unbalanced document (or one this scan misread) yields no anchors.
    if !stack.is_empty() {
        return None;
    }
    Some(anchors)
}
