//! Copy only lock-selected public Cargo cache inputs into the writable task home.
use super::*;

const INDEX: &str = "index.crates.io-1949cf8c6b5b557f";
const CONFIG: &[u8] = br#"{"dl":"https://static.crates.io/crates","api":"https://crates.io"}"#;
const PUBLIC_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

pub(crate) fn seed_cargo_cache(
    cache: &NativeDirectoryGrant,
    source: &Workspace,
    task: &Dir,
    home: &Path,
    limits: TaskWorkspaceLimits,
    stopped: &dyn Fn() -> bool,
) -> Result<(), TaskWorkspaceError> {
    if stopped() {
        return Err(TaskWorkspaceError::Stopped);
    }
    let Some(lock) = read_file(source.root(), Path::new("Cargo.lock"), 1024 * 1024)? else {
        return Ok(());
    };
    // A broken lockfile is still an editable project. It grants no cache input.
    let Some(packages) = std::str::from_utf8(&lock.bytes)
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|document| {
            document
                .get("package")
                .and_then(toml_edit::Item::as_array_of_tables)
                .cloned()
        })
    else {
        return Ok(());
    };
    if packages.len() > limits.max_entries {
        return Err(TaskWorkspaceError::LimitExceeded);
    }
    let cache_root = at(&cache.path, open_dir_ambient(&cache.path))?;
    if at(&cache.path, NativeWorkspaceIdentity::held(&cache_root))? != cache.identity {
        return Err(TaskWorkspaceError::Changed(cache.path.clone()));
    }
    let cargo = home.join(".cargo");
    let parent = at(&cargo, file_parent(task, &cargo.join("placeholder"), true))?;
    let mut remaining = limits
        .max_bytes
        .checked_sub(CONFIG.len() as u64)
        .ok_or(TaskWorkspaceError::LimitExceeded)?;
    let mut copied = BTreeSet::new();
    let mut entries = BTreeSet::new();
    for package in packages {
        if stopped() {
            return Err(TaskWorkspaceError::Stopped);
        }
        if package.get("source").and_then(toml_edit::Item::as_str) != Some(PUBLIC_SOURCE) {
            continue;
        }
        let (Some(name), Some(version), Some(checksum)) = (
            package.get("name").and_then(toml_edit::Item::as_str),
            package.get("version").and_then(toml_edit::Item::as_str),
            package.get("checksum").and_then(toml_edit::Item::as_str),
        ) else {
            continue;
        };
        if !component(name)
            || !component(version)
            || checksum.len() != 64
            || !checksum.bytes().all(|b| b.is_ascii_hexdigit())
        {
            continue;
        }
        let archive = PathBuf::from(format!("registry/cache/{INDEX}/{name}-{version}.crate"));
        if !copied.insert(archive.clone()) {
            continue;
        }
        // Missing public dependencies stay missing; ordinary Cargo can report them.
        if !cached(&cache_root, &archive)? {
            continue;
        }
        admit_entries(&mut entries, &archive, limits.max_entries)?;
        let version = copy_file(&cache_root, &parent, &archive, &mut remaining, stopped)?;
        let hex = b"0123456789abcdef";
        let checksum_matches = version
            .sha256
            .iter()
            .flat_map(|byte| [hex[(byte >> 4) as usize], hex[(byte & 15) as usize]])
            .eq(checksum.bytes().map(|byte| byte.to_ascii_lowercase()));
        if !checksum_matches {
            return Err(TaskWorkspaceError::Changed(archive));
        }
        let name = name.to_ascii_lowercase();
        let prefix = match name.len() {
            1 => "1".to_owned(),
            2 => "2".to_owned(),
            3 => format!("3/{}", &name[..1]),
            _ => format!("{}/{}", &name[..2], &name[2..4]),
        };
        let entry = PathBuf::from(format!("registry/index/{INDEX}/.cache/{prefix}/{name}"));
        if copied.insert(entry.clone()) && cached(&cache_root, &entry)? {
            admit_entries(&mut entries, &entry, limits.max_entries)?;
            copy_file(&cache_root, &parent, &entry, &mut remaining, stopped)?;
        }
    }
    if !copied.is_empty() {
        let path = PathBuf::from(format!("registry/index/{INDEX}/config.json"));
        admit_entries(&mut entries, &path, limits.max_entries)?;
        let directory = at(&path, file_parent(&parent, &path, true))?;
        let mut file = at(
            &path,
            directory.open_with(
                "config.json",
                OpenOptions::new().write(true).create_new(true),
            ),
        )?;
        at(&path, file.write_all(CONFIG))?;
    }
    Ok(())
}
fn cached(root: &Dir, path: &Path) -> Result<bool, TaskWorkspaceError> {
    match file_parent(root, path, false)
        .and_then(|parent| parent.symlink_metadata(path.file_name().expect("fixed cache path")))
    {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(TaskWorkspaceError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn admit_entries(
    entries: &mut BTreeSet<PathBuf>,
    path: &Path,
    limit: usize,
) -> Result<(), TaskWorkspaceError> {
    for path in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        entries.insert(path.to_owned());
        if entries.len() > limit {
            return Err(TaskWorkspaceError::LimitExceeded);
        }
    }
    Ok(())
}

fn component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
        && !matches!(value, "." | "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        _root: tempfile::TempDir,
        source: Workspace,
        cache: NativeDirectoryGrant,
        task: Dir,
        archive: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            for name in ["source", "cache", "task"] {
                std::fs::create_dir(root.path().join(name)).unwrap();
            }
            let source = Workspace::open(&root.path().join("source")).unwrap();
            let cache = NativeDirectoryGrant::capture(&root.path().join("cache")).unwrap();
            let task = open_dir_ambient(&root.path().join("task")).unwrap();
            let bytes = b"public archive bytes";
            let checksum = format!("{:x}", Sha256::digest(bytes));
            source.root().write("Cargo.lock", format!("version=4\n[[package]]\nname='itoa'\nversion='1.0.18'\nsource='{PUBLIC_SOURCE}'\nchecksum='{checksum}'\n[[package]]\nname='private'\nversion='1.0.0'\nsource='registry+https://private.invalid/index'\nchecksum='{checksum}'\n")).unwrap();
            let archive = PathBuf::from(format!("registry/cache/{INDEX}/itoa-1.0.18.crate"));
            let cache_root = open_dir_ambient(&cache.path).unwrap();
            file_parent(&cache_root, &archive, true)
                .unwrap()
                .write(archive.file_name().unwrap(), bytes)
                .unwrap();
            let index = PathBuf::from(format!("registry/index/{INDEX}/.cache/it/oa/itoa"));
            file_parent(&cache_root, &index, true)
                .unwrap()
                .write(index.file_name().unwrap(), b"public selected sparse entry")
                .unwrap();
            cache_root
                .write("credentials.toml", b"do not copy")
                .unwrap();
            cache_root.write("config.toml", b"do not copy").unwrap();
            Self {
                _root: root,
                source,
                cache,
                task,
                archive,
            }
        }
        fn seed(
            &self,
            max_bytes: u64,
            stopped: &dyn Fn() -> bool,
        ) -> Result<(), TaskWorkspaceError> {
            seed_cargo_cache(
                &self.cache,
                &self.source,
                &self.task,
                Path::new("home"),
                TaskWorkspaceLimits {
                    max_entries: 128,
                    max_bytes,
                },
                stopped,
            )
        }
    }
    #[test]
    fn selected_public_cargo_inputs_are_independent_and_exclude_credentials() {
        let fixture = Fixture::new();
        fixture.seed(4096, &|| false).unwrap();
        let copied = PathBuf::from("home/.cargo").join(&fixture.archive);
        assert_eq!(fixture.task.read(&copied).unwrap(), b"public archive bytes");
        assert!(!fixture.task.exists("home/.cargo/credentials.toml"));
        assert!(!fixture.task.exists("home/.cargo/config.toml"));
        assert!(!fixture.task.exists(format!(
            "home/.cargo/registry/cache/{INDEX}/private-1.0.0.crate"
        )));
        let mut options = OpenOptions::new();
        options.write(true).truncate(true);
        fixture
            .task
            .open_with(&copied, &options)
            .unwrap()
            .write_all(b"task mutation")
            .unwrap();
        assert_eq!(
            std::fs::read(fixture.cache.path.join(&fixture.archive)).unwrap(),
            b"public archive bytes"
        );
    }
    #[test]
    fn cargo_cache_refuses_checksum_mismatch_and_enforces_copy_limits() {
        let fixture = Fixture::new();
        assert!(matches!(
            fixture.seed(1, &|| false),
            Err(TaskWorkspaceError::LimitExceeded)
        ));
        assert!(matches!(
            fixture.seed(4096, &|| true),
            Err(TaskWorkspaceError::Stopped)
        ));
        std::fs::write(
            fixture.cache.path.join(&fixture.archive),
            b"tampered archive",
        )
        .unwrap();
        assert!(matches!(
            fixture.seed(4096, &|| false),
            Err(TaskWorkspaceError::Changed(_))
        ));
    }
    #[test]
    fn cargo_cache_counts_created_directories_and_skips_broken_lockfiles() {
        let fixture = Fixture::new();
        assert!(matches!(
            seed_cargo_cache(
                &fixture.cache,
                &fixture.source,
                &fixture.task,
                Path::new("bounded"),
                TaskWorkspaceLimits {
                    max_entries: 3,
                    max_bytes: 4096
                },
                &|| false,
            ),
            Err(TaskWorkspaceError::LimitExceeded)
        ));
        fixture
            .source
            .root()
            .write("Cargo.lock", b"not valid lock TOML {{{")
            .unwrap();
        seed_cargo_cache(
            &fixture.cache,
            &fixture.source,
            &fixture.task,
            Path::new("broken"),
            TaskWorkspaceLimits {
                max_entries: 128,
                max_bytes: 4096,
            },
            &|| false,
        )
        .unwrap();
        assert!(!fixture.task.exists("broken/.cargo"));
    }
    #[cfg(unix)]
    #[test]
    fn cargo_cache_refuses_archive_and_parent_aliases() {
        let fixture = Fixture::new();
        let archive = fixture.cache.path.join(&fixture.archive);
        let private = fixture.cache.path.join("credentials.toml");
        std::fs::remove_file(&archive).unwrap();
        std::os::unix::fs::symlink(&private, &archive).unwrap();
        assert!(fixture.seed(4096, &|| false).is_err());
        std::fs::remove_file(&archive).unwrap();
        std::fs::hard_link(&private, &archive).unwrap();
        assert!(fixture.seed(4096, &|| false).is_err());
        std::fs::remove_file(&archive).unwrap();
        let parent = archive.parent().unwrap();
        std::fs::remove_dir(parent).unwrap();
        std::os::unix::fs::symlink(fixture.cache.path.join("registry/index"), parent).unwrap();
        assert!(fixture.seed(4096, &|| false).is_err());
    }
}
