use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use super::{
    ConfigError, ConfigLoader, ConfigPaths, ConfigSnapshot, ConfigSources, LoadRequest,
    MAX_CONFIG_BYTES, PendingTrust, SourceIdentity, SourceKind, SourceReport, SourceStatus,
    document::{Document, MergeState},
    managed::MdmConfiguration,
    remote,
};

pub(super) fn system_paths() -> Result<ConfigPaths, ConfigError> {
    let project =
        ProjectDirs::from("dev", "qq", "qq").ok_or(ConfigError::SystemDirectoriesUnavailable)?;
    Ok(ConfigPaths::new(
        project.config_dir(),
        project.data_dir(),
        system_managed_dir(),
    )
    .with_managed_ownership_checks())
}

#[cfg(target_os = "linux")]
fn system_managed_dir() -> PathBuf {
    PathBuf::from("/etc/qq")
}

#[cfg(target_os = "macos")]
fn system_managed_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/qq")
}

#[cfg(target_os = "windows")]
fn system_managed_dir() -> PathBuf {
    use directories::BaseDirs;

    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .or_else(|| {
            BaseDirs::new().and_then(|base| {
                base.home_dir()
                    .ancestors()
                    .last()
                    .map(|root| root.join("ProgramData"))
            })
        })
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("qq")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn system_managed_dir() -> PathBuf {
    PathBuf::from("/etc/qq")
}

pub(super) fn load(
    loader: &ConfigLoader,
    request: &LoadRequest,
) -> Result<ConfigSnapshot, ConfigError> {
    let mut probes = Probes::default();
    let probes = &mut probes;
    probes.record(&request.cwd);
    let cwd = canonical_working_directory(&request.cwd)?;
    probes.record(&cwd);
    let trust = TrustState::load(&loader.paths, probes)?;
    let mdm = read_mdm_document(loader)?;
    let organization = selected_organization(loader, request, &cwd, &trust, mdm.as_ref(), probes)?;
    let (mut merged, compiled_report) = MergeState::compiled();
    let mut report = LoadReport {
        sources: vec![compiled_report],
        pending: Vec::new(),
    };
    let mut seen = BTreeSet::new();
    let mut admitted_packs = 0_usize;

    // Discovered packs are the lowest lane: global first, then each project
    // directory root-to-leaf, so a nearer pack of the same id replaces a
    // farther one. Explicit `packs` entries in any layer apply as that layer
    // is merged (below) and therefore win over discovery.
    for pack in crate::pack::discover(
        &loader.paths.global_dir.join("packs"),
        SourceKind::Global,
        probes,
        &mut admitted_packs,
    )? {
        merged.admit_pack(pack);
    }

    if let Some(organization) = organization
        && let Some((document, source)) =
            remote::load_cached_if_enrolled(&loader.paths, &organization, probes)?
    {
        apply_document(document, source, &trust, &mut merged, &mut report, probes)?;
    }

    for candidate in discover_layer_directory(
        &loader.paths.global_dir,
        "config.ron",
        "config.d",
        SourceKind::Global,
        probes,
    )? {
        apply_candidate(
            candidate,
            false,
            &mut seen,
            &trust,
            &mut merged,
            &mut report,
            probes,
        )?;
    }

    for directory in project_directories(&cwd, probes) {
        // Project packs are sensitive (they may declare MCP servers), so
        // they are admitted only when the project's configuration is
        // trusted, exactly like `mcp` in `.qq/config.ron`. An untrusted
        // project contributes no packs and no pending-trust entry of its
        // own: trusting the directory's configuration admits them.
        let project_trusted = trust.project_trusted(&directory, probes)?;
        if project_trusted {
            for pack in crate::pack::discover(
                &directory.join(".qq").join("packs"),
                SourceKind::Project,
                probes,
                &mut admitted_packs,
            )? {
                merged.admit_pack(pack);
            }
        } else {
            probes.record(&directory.join(".qq").join("packs"));
        }
        if let Some(candidate) =
            discover_file(directory.join("qq.ron"), SourceKind::Project, false, probes)?
        {
            apply_candidate(
                candidate,
                false,
                &mut seen,
                &trust,
                &mut merged,
                &mut report,
                probes,
            )?;
        }
        for candidate in discover_layer_directory(
            &directory.join(".qq"),
            "config.ron",
            "config.d",
            SourceKind::Project,
            probes,
        )? {
            apply_candidate(
                candidate,
                false,
                &mut seen,
                &trust,
                &mut merged,
                &mut report,
                probes,
            )?;
        }
    }

    if let Some(path) = &request.explicit_path {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let candidate = discover_file(path.clone(), SourceKind::Explicit, true, probes)?
            .ok_or(ConfigError::ExplicitConfigMissing { path })?;
        apply_candidate(
            candidate,
            false,
            &mut seen,
            &trust,
            &mut merged,
            &mut report,
            probes,
        )?;
    }

    if let Some(content) = &request.explicit_content {
        if content.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::InlineSourceTooLarge {
                limit: MAX_CONFIG_BYTES,
            });
        }
        let source = SourceIdentity::virtual_source(SourceKind::Inline, "QQ_CONFIG_CONTENT");
        apply_document(
            Document::parse(content, &source)?,
            source,
            &trust,
            &mut merged,
            &mut report,
            probes,
        )?;
    }

    if !request.overrides.is_empty() {
        let source = SourceIdentity::virtual_source(SourceKind::Runtime, "runtime overrides");
        let touched = merged.apply_runtime(&request.overrides, &source);
        report
            .sources
            .push(SourceReport::new(source, SourceStatus::Applied, touched));
    }

    // Administrator-owned files and native MDM values intentionally run last.
    probes.record(&loader.paths.managed_dir);
    if loader.paths.enforce_managed_ownership {
        validate_managed_directory_if_present(&loader.paths.managed_dir)?;
    }
    for candidate in discover_layer_directory(
        &loader.paths.managed_dir,
        "managed.ron",
        "managed.d",
        SourceKind::Managed,
        probes,
    )? {
        apply_candidate(
            candidate,
            loader.paths.enforce_managed_ownership,
            &mut seen,
            &trust,
            &mut merged,
            &mut report,
            probes,
        )?;
    }

    if let Some(mdm) = mdm {
        apply_document(
            mdm.document,
            mdm.source,
            &trust,
            &mut merged,
            &mut report,
            probes,
        )?;
    }

    if !report.pending.is_empty() {
        return Err(ConfigError::TrustRequired {
            pending: report.pending,
            reports: report.sources,
        });
    }
    merged.finish(report.sources, std::mem::take(probes).into_sources())
}

fn selected_organization(
    loader: &ConfigLoader,
    request: &LoadRequest,
    cwd: &Path,
    trust: &TrustState,
    mdm: Option<&MdmDocument>,
    probes: &mut Probes,
) -> Result<Option<String>, ConfigError> {
    let mut organization = remote::selected(&loader.paths, probes)?;
    let mut seen = BTreeSet::new();

    for candidate in discover_layer_directory(
        &loader.paths.global_dir,
        "config.ron",
        "config.d",
        SourceKind::Global,
        probes,
    )? {
        apply_organization_candidate(candidate, false, &mut seen, trust, &mut organization)?;
    }

    for directory in project_directories(cwd, probes) {
        if let Some(candidate) =
            discover_file(directory.join("qq.ron"), SourceKind::Project, false, probes)?
        {
            apply_organization_candidate(candidate, false, &mut seen, trust, &mut organization)?;
        }
        for candidate in discover_layer_directory(
            &directory.join(".qq"),
            "config.ron",
            "config.d",
            SourceKind::Project,
            probes,
        )? {
            apply_organization_candidate(candidate, false, &mut seen, trust, &mut organization)?;
        }
    }

    if let Some(path) = &request.explicit_path {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let candidate = discover_file(path.clone(), SourceKind::Explicit, true, probes)?
            .ok_or(ConfigError::ExplicitConfigMissing { path })?;
        apply_organization_candidate(candidate, false, &mut seen, trust, &mut organization)?;
    }

    if let Some(content) = &request.explicit_content {
        if content.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::InlineSourceTooLarge {
                limit: MAX_CONFIG_BYTES,
            });
        }
        let source = SourceIdentity::virtual_source(SourceKind::Inline, "QQ_CONFIG_CONTENT");
        Document::parse(content, &source)?.apply_organization(&mut organization);
    }

    if let Some(selected) = request.overrides.organization() {
        organization = Some(selected.to_owned());
    }

    probes.record(&loader.paths.managed_dir);
    if loader.paths.enforce_managed_ownership {
        validate_managed_directory_if_present(&loader.paths.managed_dir)?;
    }
    for candidate in discover_layer_directory(
        &loader.paths.managed_dir,
        "managed.ron",
        "managed.d",
        SourceKind::Managed,
        probes,
    )? {
        apply_organization_candidate(
            candidate,
            loader.paths.enforce_managed_ownership,
            &mut seen,
            trust,
            &mut organization,
        )?;
    }

    if let Some(mdm) = mdm {
        mdm.document.apply_organization(&mut organization);
    }

    Ok(organization)
}

pub(super) struct MdmDocument {
    source: SourceIdentity,
    pub(super) document: Document,
}

pub(super) fn read_mdm_document(loader: &ConfigLoader) -> Result<Option<MdmDocument>, ConfigError> {
    let Some(MdmConfiguration { origin, content }) = loader.mdm_reader.read()? else {
        return Ok(None);
    };
    let source = SourceIdentity::virtual_source(SourceKind::Mdm, origin);
    if content.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::SourceTooLarge {
            origin: source,
            limit: MAX_CONFIG_BYTES,
        });
    }
    let document = Document::parse(&content, &source)?;
    Ok(Some(MdmDocument { source, document }))
}

fn apply_organization_candidate(
    candidate: FileCandidate,
    enforce_managed_ownership: bool,
    seen: &mut BTreeSet<PathBuf>,
    trust: &TrustState,
    organization: &mut Option<String>,
) -> Result<(), ConfigError> {
    if !seen.insert(candidate.path.clone()) {
        return Err(ConfigError::DuplicateSource {
            path: candidate.path,
        });
    }
    if enforce_managed_ownership {
        validate_managed_file(&candidate.path)?;
    }
    let (source, content) = read_candidate(&candidate)?;
    let document = Document::parse(&content, &source)?;
    if source.kind() == SourceKind::Global && document.contains_literal_secret() {
        validate_private_secret_file(
            source
                .path()
                .expect("global file sources always have a path"),
        )?;
    }
    let trusted = source.kind() != SourceKind::Project
        || document.sensitive_digest()?.is_none_or(|digest| {
            trust.contains(source.path().expect("project source path"), &digest)
        });
    if trusted {
        document.apply_organization(organization);
    }
    Ok(())
}

pub(super) fn grant_pending_trust(
    loader: &ConfigLoader,
    request: &LoadRequest,
) -> Result<Vec<PendingTrust>, ConfigError> {
    let cwd = canonical_working_directory(&request.cwd)?;
    let _state_lock = TrustStateLock::acquire(&loader.paths)?;
    let mut probes = Probes::default();
    let probes = &mut probes;
    let mut trust = TrustState::load(&loader.paths, probes)?;
    let mut pending = Vec::new();

    for directory in project_directories(&cwd, probes) {
        let mut candidates = Vec::new();
        if let Some(candidate) =
            discover_file(directory.join("qq.ron"), SourceKind::Project, false, probes)?
        {
            candidates.push(candidate);
        }
        candidates.extend(discover_layer_directory(
            &directory.join(".qq"),
            "config.ron",
            "config.d",
            SourceKind::Project,
            probes,
        )?);

        for candidate in candidates {
            let (source, content) = read_candidate(&candidate)?;
            let document = Document::parse(&content, &source)?;
            let Some(digest) = document.sensitive_digest()? else {
                continue;
            };
            let path = source
                .path()
                .expect("project file sources always have a canonical path");
            if !trust.contains(path, &digest) {
                pending.push(PendingTrust::new(source, digest));
            }
        }
    }

    if !pending.is_empty() {
        for item in &pending {
            trust.insert(
                item.source()
                    .path()
                    .expect("pending project trust always has a path")
                    .to_owned(),
                item.digest().to_owned(),
            );
        }
        trust.save(&loader.paths)?;
    }
    Ok(pending)
}

/// The filesystem locations one load inspected to decide which sources exist.
/// Recording them lets a caller revalidate a cached result by re-checking
/// exactly these paths instead of rediscovering: every directory whose
/// presence or listing mattered, every candidate file whether or not it
/// existed, and every VCS root marker probed while bounding the project walk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Probes {
    paths: Vec<PathBuf>,
    states: Vec<SourceState>,
}

impl Probes {
    pub(super) fn record(&mut self, path: &Path) {
        if !self.paths.iter().any(|recorded| recorded == path) {
            // Retain the first observation, before discovery or a read. A
            // later probe must not certify changed bytes as the old input.
            self.states.push(SourceState::capture(path));
            self.paths.push(path.to_owned());
        }
    }

    pub(super) fn into_sources(self) -> ConfigSources {
        ConfigSources(std::sync::Arc::new(self))
    }

    pub(super) fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub(super) fn is_current(&self) -> bool {
        self.paths.iter().zip(&self.states).all(|(path, state)| {
            !matches!(state.entry, MetadataState::Unreadable(_))
                && !matches!(state.target, Some(MetadataState::Unreadable(_)))
                && SourceState::capture(path) == *state
        })
    }

    pub(super) fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + 2 * std::mem::size_of::<usize>()
            + self.paths.capacity() * std::mem::size_of::<PathBuf>()
            + self.states.capacity() * std::mem::size_of::<SourceState>()
            + self.paths.iter().map(PathBuf::capacity).sum::<usize>()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceState {
    entry: MetadataState,
    target: Option<MetadataState>,
}

impl SourceState {
    fn capture(path: &Path) -> Self {
        let metadata = fs::symlink_metadata(path);
        // Configuration files reject links, but VCS marker discovery follows
        // them. Retain target state too so that decision stays revalidatable.
        let target = metadata
            .as_ref()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
            .then(|| MetadataState::observe(fs::metadata(path)));
        Self {
            entry: MetadataState::observe(metadata),
            target,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MetadataState {
    Absent,
    Present {
        len: u64,
        modified: Option<SystemTime>,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
        #[cfg(unix)]
        mode: u32,
        #[cfg(unix)]
        uid: u32,
        is_dir: bool,
    },
    Unreadable(std::io::ErrorKind),
}

impl MetadataState {
    fn observe(metadata: std::io::Result<fs::Metadata>) -> Self {
        match metadata {
            Ok(metadata) => {
                #[cfg(unix)]
                use std::os::unix::fs::MetadataExt;
                Self::Present {
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                    #[cfg(unix)]
                    device: metadata.dev(),
                    #[cfg(unix)]
                    inode: metadata.ino(),
                    #[cfg(unix)]
                    mode: metadata.mode(),
                    #[cfg(unix)]
                    uid: metadata.uid(),
                    is_dir: metadata.is_dir(),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Absent,
            Err(error) => Self::Unreadable(error.kind()),
        }
    }
}

#[cfg(test)]
thread_local! {
    static REWRITE_AFTER_READ: std::cell::RefCell<Option<(PathBuf, usize, String)>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(super) fn rewrite_after_read(path: PathBuf, read: usize, content: String) {
    REWRITE_AFTER_READ.with(|rewrite| {
        assert!(
            rewrite
                .borrow_mut()
                .replace((path, read, content))
                .is_none()
        );
    });
}

#[derive(Clone, Debug)]
pub(super) struct FileCandidate {
    pub(super) path: PathBuf,
    kind: SourceKind,
}

struct LoadReport {
    sources: Vec<SourceReport>,
    pending: Vec<PendingTrust>,
}

fn apply_candidate(
    candidate: FileCandidate,
    enforce_managed_ownership: bool,
    seen: &mut BTreeSet<PathBuf>,
    trust: &TrustState,
    merged: &mut MergeState,
    report: &mut LoadReport,
    probes: &mut Probes,
) -> Result<(), ConfigError> {
    if !seen.insert(candidate.path.clone()) {
        return Err(ConfigError::DuplicateSource {
            path: candidate.path,
        });
    }
    if enforce_managed_ownership {
        validate_managed_file(&candidate.path)?;
    }
    let (source, content) = read_candidate(&candidate)?;
    let document = Document::parse(&content, &source)?;
    if source.kind() == SourceKind::Global && document.contains_literal_secret() {
        validate_private_secret_file(
            source
                .path()
                .expect("global file sources always have a path"),
        )?;
    }
    apply_document(document, source, trust, merged, report, probes)
}

fn apply_document(
    document: Document,
    source: SourceIdentity,
    trust: &TrustState,
    merged: &mut MergeState,
    report: &mut LoadReport,
    probes: &mut Probes,
) -> Result<(), ConfigError> {
    let pending_digest = if source.kind() == SourceKind::Project {
        document
            .sensitive_digest()?
            .filter(|digest| !trust.contains(source.path().expect("project source path"), digest))
    } else {
        None
    };
    let sensitive = pending_digest.is_none();
    merged.apply_document(&document, &source, sensitive);
    if sensitive {
        apply_explicit_packs(&document, &source, merged, probes)?;
    }
    let status = if let Some(digest) = pending_digest {
        report
            .pending
            .push(PendingTrust::new(source.clone(), digest));
        SourceStatus::PartiallyAppliedPendingTrust
    } else {
        SourceStatus::Applied
    };
    report
        .sources
        .push(SourceReport::new(source, status, document.touched()));
    Ok(())
}

/// Applies a layer's explicit `packs` entries. Paths resolve relative to the
/// declaring file; virtual sources (inline, MDM, remote) may only use
/// absolute paths. Discovery is not repeated here: an explicit entry names
/// exactly one directory.
fn apply_explicit_packs(
    document: &Document,
    source: &SourceIdentity,
    merged: &mut MergeState,
    probes: &mut Probes,
) -> Result<(), ConfigError> {
    use crate::document::{Field, PackPatch};
    match document.packs() {
        Field::Missing => Ok(()),
        Field::Clear => {
            merged.clear_packs();
            Ok(())
        }
        Field::Set(patches) => {
            for (id, patch) in &patches.0 {
                match patch {
                    PackPatch::Remove => merged.remove_pack(id),
                    PackPatch::Pack { path } => {
                        let path = Path::new(path);
                        let resolved = if path.is_absolute() {
                            path.to_owned()
                        } else if let Some(declaring) = source.path().and_then(Path::parent) {
                            declaring.join(path)
                        } else {
                            return Err(ConfigError::InvalidPack {
                                origin: source.clone(),
                                message: format!(
                                    "pack {id:?} path must be absolute in a {:?} source",
                                    source.kind()
                                ),
                            });
                        };
                        let manifest = resolved.join(crate::pack::PACK_MANIFEST_FILE);
                        probes.record(&manifest);
                        if !manifest.is_file() {
                            return Err(ConfigError::PackMissing {
                                id: id.clone(),
                                path: resolved,
                            });
                        }
                        let pack =
                            crate::pack::load_explicit(&resolved, id, source.kind(), probes)?;
                        merged.admit_pack(pack);
                    }
                }
            }
            Ok(())
        }
    }
}

/// Canonicalizes a working directory the way every configuration load does,
/// rejecting paths that do not name an existing directory.
pub fn canonical_working_directory(path: &Path) -> Result<PathBuf, ConfigError> {
    let canonical = fs::canonicalize(path).map_err(|_| ConfigError::InvalidWorkingDirectory {
        path: path.to_owned(),
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| ConfigError::Io {
        path: canonical.clone(),
        error,
    })?;
    if !metadata.is_dir() {
        return Err(ConfigError::InvalidWorkingDirectory { path: canonical });
    }
    Ok(canonical)
}

pub(super) fn project_directories(cwd: &Path, probes: &mut Probes) -> Vec<PathBuf> {
    let root = cwd
        .ancestors()
        .find(|directory| is_vcs_root(directory, probes))
        .unwrap_or(cwd);
    let mut directories = Vec::new();
    let mut current = Some(cwd);
    while let Some(directory) = current {
        directories.push(directory.to_owned());
        if directory == root {
            break;
        }
        current = directory.parent();
    }
    directories.reverse();
    directories
}

fn is_vcs_root(directory: &Path, probes: &mut Probes) -> bool {
    for marker in [".git", ".hg", ".svn"] {
        let candidate = directory.join(marker);
        probes.record(&candidate);
        if candidate.exists() {
            return true;
        }
    }
    false
}

pub(super) fn discover_layer_directory(
    directory: &Path,
    primary_name: &str,
    fragment_name: &str,
    kind: SourceKind,
    probes: &mut Probes,
) -> Result<Vec<FileCandidate>, ConfigError> {
    if !check_optional_directory(directory, probes)? {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    if let Some(primary) = discover_file(directory.join(primary_name), kind, false, probes)? {
        candidates.push(primary);
    }
    candidates.extend(discover_fragments(
        &directory.join(fragment_name),
        kind,
        probes,
    )?);
    Ok(candidates)
}

fn discover_fragments(
    directory: &Path,
    kind: SourceKind,
    probes: &mut Probes,
) -> Result<Vec<FileCandidate>, ConfigError> {
    if !check_optional_directory(directory, probes)? {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(directory).map_err(|error| ConfigError::Io {
        path: directory.to_owned(),
        error,
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ConfigError::Io {
            path: directory.to_owned(),
            error,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_fragment_name(name) {
            paths.push((name.to_owned(), entry.path()));
        } else if name.ends_with(".ron") {
            return Err(ConfigError::InvalidFragmentName { path: entry.path() });
        }
    }
    paths.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut candidates = Vec::new();
    for (_, path) in paths {
        if let Some(candidate) = discover_file(path, kind, false, probes)? {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn is_fragment_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 8
        || !bytes[0].is_ascii_digit()
        || !bytes[1].is_ascii_digit()
        || bytes[2] != b'-'
        || !name.ends_with(".ron")
    {
        return false;
    }
    let stem = &bytes[3..bytes.len() - 4];
    !stem.is_empty()
        && stem.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn check_optional_directory(path: &Path, probes: &mut Probes) -> Result<bool, ConfigError> {
    probes.record(path);
    reject_symlink_components(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(ConfigError::Io {
                path: path.to_owned(),
                error,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(ConfigError::SymlinkSource {
            path: path.to_owned(),
        });
    }
    if !metadata.is_dir() {
        return Err(ConfigError::NotDirectory {
            path: path.to_owned(),
        });
    }
    Ok(true)
}

pub(super) fn discover_file(
    path: PathBuf,
    kind: SourceKind,
    required: bool,
    probes: &mut Probes,
) -> Result<Option<FileCandidate>, ConfigError> {
    probes.record(&path);
    reject_symlink_components(&path)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if required {
                return Err(ConfigError::ExplicitConfigMissing { path });
            }
            return Ok(None);
        }
        Err(error) => return Err(ConfigError::Io { path, error }),
    };
    if metadata.file_type().is_symlink() {
        return Err(ConfigError::SymlinkSource { path });
    }
    if !metadata.is_file() {
        return Err(ConfigError::NotRegularFile { path });
    }
    let canonical = fs::canonicalize(&path).map_err(|error| ConfigError::Io {
        path: path.clone(),
        error,
    })?;
    Ok(Some(FileCandidate {
        path: canonical,
        kind,
    }))
}

pub(super) fn reject_symlink_components(path: &Path) -> Result<(), ConfigError> {
    let mut components: Vec<_> = path.ancestors().collect();
    components.reverse();
    for component in components {
        if component.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ConfigError::SymlinkSource {
                    path: component.to_owned(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ConfigError::Io {
                    path: component.to_owned(),
                    error,
                });
            }
        }
    }
    Ok(())
}

pub(super) fn read_candidate(
    candidate: &FileCandidate,
) -> Result<(SourceIdentity, String), ConfigError> {
    let metadata = fs::symlink_metadata(&candidate.path).map_err(|error| ConfigError::Io {
        path: candidate.path.clone(),
        error,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ConfigError::SymlinkSource {
            path: candidate.path.clone(),
        });
    }
    if !metadata.is_file() {
        return Err(ConfigError::NotRegularFile {
            path: candidate.path.clone(),
        });
    }
    let source = SourceIdentity::file(candidate.kind, candidate.path.clone());
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(ConfigError::SourceTooLarge {
            origin: source,
            limit: MAX_CONFIG_BYTES,
        });
    }

    let file = File::open(&candidate.path).map_err(|error| ConfigError::Io {
        path: candidate.path.clone(),
        error,
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigError::Io {
            path: candidate.path.clone(),
            error,
        })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::SourceTooLarge {
            origin: source,
            limit: MAX_CONFIG_BYTES,
        });
    }
    let content = String::from_utf8(bytes).map_err(|_| ConfigError::InvalidUtf8 {
        origin: source.clone(),
    })?;
    #[cfg(test)]
    REWRITE_AFTER_READ.with(|rewrite| {
        let mut rewrite = rewrite.borrow_mut();
        if let Some((path, reads_left, _)) = rewrite.as_mut()
            && *path == candidate.path
        {
            *reads_left -= 1;
            if *reads_left == 0 {
                let (path, _, content) = rewrite.take().unwrap();
                fs::write(path, content).unwrap();
            }
        }
    });
    Ok((source, content))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustRecord {
    path: PathBuf,
    digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TrustState {
    version: u32,
    records: Vec<TrustRecord>,
}

impl Default for TrustState {
    fn default() -> Self {
        Self {
            version: 1,
            records: Vec::new(),
        }
    }
}

impl TrustState {
    pub(super) fn load(paths: &ConfigPaths, probes: &mut Probes) -> Result<Self, ConfigError> {
        let Some(candidate) =
            discover_file(paths.trust_file(), SourceKind::TrustState, false, probes)?
        else {
            return Ok(Self::default());
        };
        let (source, content) = read_candidate(&candidate)?;
        let mut state: Self = ron::from_str(&content).map_err(|error| ConfigError::Parse {
            origin: source,
            message: error.to_string(),
        })?;
        if state.version != 1 {
            return Err(ConfigError::UnsupportedTrustVersion {
                version: state.version,
            });
        }

        let mut unique = BTreeSet::new();
        for record in &state.records {
            if !is_sha256_digest(&record.digest) {
                return Err(ConfigError::InvalidTrustDigest {
                    digest: record.digest.clone(),
                });
            }
            if !unique.insert((record.path.clone(), record.digest.clone())) {
                return Err(ConfigError::DuplicateTrustRecord {
                    path: record.path.clone(),
                    digest: record.digest.clone(),
                });
            }
        }
        state
            .records
            .sort_by(|left, right| (&left.path, &left.digest).cmp(&(&right.path, &right.digest)));
        Ok(state)
    }

    pub(super) fn contains(&self, path: &Path, digest: &str) -> bool {
        self.records
            .iter()
            .any(|record| record.path == path && record.digest == digest)
    }

    /// Whether every sensitive configuration file under `directory` (its
    /// `qq.ron` and `.qq/config.ron` plus fragments) is trusted, or none
    /// declares anything sensitive. Packs under the directory follow this
    /// decision so trusting a project admits its packs.
    pub(super) fn project_trusted(
        &self,
        directory: &Path,
        probes: &mut Probes,
    ) -> Result<bool, ConfigError> {
        let mut candidates = Vec::new();
        if let Some(candidate) =
            discover_file(directory.join("qq.ron"), SourceKind::Project, false, probes)?
        {
            candidates.push(candidate);
        }
        candidates.extend(discover_layer_directory(
            &directory.join(".qq"),
            "config.ron",
            "config.d",
            SourceKind::Project,
            probes,
        )?);
        for candidate in candidates {
            let (source, content) = read_candidate(&candidate)?;
            let document = Document::parse(&content, &source)?;
            if let Some(digest) = document.sensitive_digest()?
                && !self.contains(&candidate.path, &digest)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn insert(&mut self, path: PathBuf, digest: String) {
        if !self.contains(&path, &digest) {
            self.records.push(TrustRecord { path, digest });
        }
    }

    pub(super) fn save(&mut self, paths: &ConfigPaths) -> Result<(), ConfigError> {
        self.records
            .sort_by(|left, right| (&left.path, &left.digest).cmp(&(&right.path, &right.digest)));
        ensure_data_directory(&paths.data_dir)?;
        let content = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default()).map_err(
            |error| ConfigError::StateSerialization {
                message: error.to_string(),
            },
        )?;
        if content.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::SourceTooLarge {
                origin: SourceIdentity::file(SourceKind::TrustState, paths.trust_file()),
                limit: MAX_CONFIG_BYTES,
            });
        }
        atomic_write(&paths.trust_file(), content.as_bytes())
    }
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub(super) fn ensure_data_directory(path: &Path) -> Result<(), ConfigError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ConfigError::SymlinkSource {
                    path: path.to_owned(),
                });
            }
            if !metadata.is_dir() {
                return Err(ConfigError::NotDirectory {
                    path: path.to_owned(),
                });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path).map_err(|error| ConfigError::Io {
                path: path.to_owned(),
                error,
            })?;
            let metadata = fs::symlink_metadata(path).map_err(|error| ConfigError::Io {
                path: path.to_owned(),
                error,
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ConfigError::NotDirectory {
                    path: path.to_owned(),
                });
            }
        }
        Err(error) => {
            return Err(ConfigError::Io {
                path: path.to_owned(),
                error,
            });
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path).map_err(|error| ConfigError::Io {
            path: path.to_owned(),
            error,
        })?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ConfigError::InsecureStatePermissions {
                path: path.to_owned(),
            });
        }
    }
    Ok(())
}

pub(super) struct TrustStateLock(File);

impl TrustStateLock {
    pub(super) fn acquire(paths: &ConfigPaths) -> Result<Self, ConfigError> {
        ensure_data_directory(&paths.data_dir)?;
        let path = paths.trust_lock_file();
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|error| ConfigError::Io {
            path: path.clone(),
            error,
        })?;
        #[cfg(unix)]
        validate_private_file_mode(
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

impl Drop for TrustStateLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(unix)]
fn validate_private_secret_file(path: &Path) -> Result<(), ConfigError> {
    let metadata = fs::metadata(path).map_err(|error| ConfigError::Io {
        path: path.to_owned(),
        error,
    })?;
    validate_private_file_mode(path, &metadata)
}

#[cfg(not(unix))]
fn validate_private_secret_file(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(unix)]
pub(super) fn validate_private_file_mode(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ConfigError::InsecureSecretFile {
            path: path.to_owned(),
        });
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn validate_managed_directory_if_present(path: &Path) -> Result<(), ConfigError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_managed_metadata(path, &metadata, true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConfigError::Io {
            path: path.to_owned(),
            error,
        }),
    }
}

#[cfg(not(unix))]
pub(super) fn validate_managed_directory_if_present(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(unix)]
pub(super) fn validate_managed_file(path: &Path) -> Result<(), ConfigError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| ConfigError::Io {
        path: path.to_owned(),
        error,
    })?;
    validate_managed_metadata(path, &metadata, false)
}

#[cfg(not(unix))]
pub(super) fn validate_managed_file(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(unix)]
fn validate_managed_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    directory: bool,
) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt;
    let expected_type = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    if metadata.file_type().is_symlink()
        || !expected_type
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
    {
        return Err(ConfigError::InsecureManagedSource {
            path: path.to_owned(),
        });
    }
    Ok(())
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let parent = path.parent().ok_or_else(|| ConfigError::Io {
        path: path.to_owned(),
        error: std::io::Error::new(std::io::ErrorKind::InvalidInput, "state path has no parent"),
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|error| ConfigError::Io {
            path: temporary.clone(),
            error,
        })?;
        file.write_all(bytes).map_err(|error| ConfigError::Io {
            path: temporary.clone(),
            error,
        })?;
        file.sync_all().map_err(|error| ConfigError::Io {
            path: temporary.clone(),
            error,
        })?;
        fs::rename(&temporary, path).map_err(|error| ConfigError::Io {
            path: path.to_owned(),
            error,
        })?;
        #[cfg(unix)]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| ConfigError::Io {
                path: parent.to_owned(),
                error,
            })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
