//! Resolve bootstrap dependencies once, without loading caller-writable code.
use goblin::elf::{Elf, dynamic};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    fs::File,
    io::{self, Read as _, Seek as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub(super) struct Bootstrap {
    loader: Option<PathBuf>,
    preload: OsString,
}

fn refused(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

fn protected(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() || rustix::process::getuid().is_root() {
        return Err(refused(
            "native bootstrap dependencies require protected absolute paths and an unprivileged owner",
        ));
    }
    let canonical = path.canonicalize()?;
    for path in [path, canonical.as_path()] {
        for ancestor in path.ancestors() {
            let metadata = ancestor.symlink_metadata()?;
            if metadata.uid() != 0
                || (!metadata.file_type().is_symlink()
                    && metadata.mode() & 0o022 != 0
                    && !(metadata.is_dir() && metadata.mode() & 0o1000 != 0))
            {
                return Err(refused(format!(
                    "native bootstrap dependency is writable: {}",
                    ancestor.display()
                )));
            }
        }
    }
    Ok(canonical)
}

fn bytes(file: &mut File) -> io::Result<Vec<u8>> {
    const LIMIT: u64 = 256 * 1024 * 1024;
    file.rewind()?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(refused("native bootstrap image exceeds its bound"));
    }
    file.rewind()?;
    Ok(bytes)
}

fn parse(bytes: &[u8]) -> io::Result<Elf<'_>> {
    const DT_AUXILIARY: u64 = 0x7fff_fffd;
    const DT_FILTER: u64 = 0x7fff_ffff;
    let elf = Elf::parse(bytes)
        .map_err(|error| refused(format!("invalid native bootstrap ELF: {error}")))?;
    if elf.dynamic.as_ref().is_some_and(|table| {
        table.dyns.iter().any(|entry| {
            matches!(
                entry.d_tag,
                dynamic::DT_AUDIT
                    | dynamic::DT_DEPAUDIT
                    | DT_FILTER
                    | DT_AUXILIARY
                    | dynamic::DT_CONFIG
            )
        })
    }) {
        return Err(refused(
            "native bootstrap cannot use additional dynamic-loader modules",
        ));
    }
    Ok(elf)
}

fn search_paths(elf: &Elf<'_>, origin: &Path) -> Vec<PathBuf> {
    elf.rpaths
        .iter()
        .chain(elf.runpaths.iter())
        .flat_map(|list| list.split(':'))
        .filter_map(|path| {
            let origin = origin.parent()?.to_str()?;
            let expanded = path.replace("${ORIGIN}", origin).replace("$ORIGIN", origin);
            if expanded.contains('$') {
                return None;
            }
            let path = protected(Path::new(&expanded)).ok()?;
            path.is_dir().then_some(path)
        })
        .collect()
}

impl Bootstrap {
    pub(super) fn capture(image: &mut File, original: &Path) -> io::Result<Self> {
        let image_bytes = bytes(image)?;
        let elf = parse(&image_bytes)?;
        let Some(interpreter) = elf.interpreter else {
            if elf.libraries.is_empty() {
                return Ok(Self {
                    loader: None,
                    preload: OsString::new(),
                });
            }
            return Err(refused(
                "native bootstrap dependencies require a dynamic loader",
            ));
        };
        match std::fs::read_to_string("/etc/ld.so.preload") {
            Ok(preload)
                if preload
                    .lines()
                    .any(|line| !line.split('#').next().unwrap_or("").trim().is_empty()) =>
            {
                return Err(refused(
                    "native bootstrap cannot use system preload modules",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let loader = protected(Path::new(interpreter))?;
        if !loader
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"ld-linux"))
            || !super::no_file_capabilities(&loader)
            || loader.metadata()?.mode() & 0o6111 != 0o111
        {
            return Err(refused(
                "native bootstrap requires an unprivileged protected glibc loader or a static ELF",
            ));
        }
        let mut paths = search_paths(&elf, original);
        // Used only to resolve a finite root-owned preload closure; the loader
        // receives explicit canonical files rather than these search paths.
        for directory in [
            "/lib",
            "/lib64",
            "/usr/lib",
            "/usr/lib64",
            "/lib/x86_64-linux-gnu",
            "/usr/lib/x86_64-linux-gnu",
            "/lib/aarch64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
        ] {
            if let Ok(path) = protected(Path::new(directory)) {
                paths.push(path);
            }
        }
        let machine = elf.header.e_machine;
        let mut pending = VecDeque::from([(
            elf.libraries
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
            paths,
        )]);
        let mut libraries = BTreeMap::<String, PathBuf>::new();
        while let Some((needed, paths)) = pending.pop_front() {
            for name in needed {
                if libraries.contains_key(&name) {
                    continue;
                }
                if name.contains('/') || libraries.len() >= 128 {
                    return Err(refused("unsupported native bootstrap dependency"));
                }
                let mut selected = None;
                for directory in &paths {
                    let candidate = directory.join(&name);
                    let Ok(path) = protected(&candidate) else {
                        continue;
                    };
                    let library_bytes = bytes(&mut File::open(&path)?)?;
                    let library = parse(&library_bytes)?;
                    if library.header.e_machine != machine || library.is_64 != elf.is_64 {
                        continue;
                    }
                    if library.soname != Some(name.as_str()) {
                        return Err(refused(
                            "native bootstrap dependency must have a matching SONAME",
                        ));
                    }
                    let mut child_paths = search_paths(&library, &path);
                    child_paths.extend(paths.iter().cloned());
                    let needed = library
                        .libraries
                        .iter()
                        .map(|name| (*name).to_owned())
                        .collect();
                    selected = Some((path, needed, child_paths));
                    break;
                }
                let Some((path, needed, paths)) = selected else {
                    return Err(refused(format!(
                        "native bootstrap dependency is not protected: {name}"
                    )));
                };
                libraries.insert(name, path);
                pending.push_back((needed, paths));
            }
        }
        let mut preload = Vec::new();
        for path in libraries.values().filter(|path| **path != loader) {
            let path = path
                .to_str()
                .ok_or_else(|| refused("native bootstrap dependency path must be UTF-8"))?;
            if path
                .bytes()
                .any(|byte| byte == b':' || byte.is_ascii_whitespace())
            {
                return Err(refused(
                    "native bootstrap dependency path contains a loader separator",
                ));
            }
            preload.push(path);
        }
        Ok(Self {
            loader: Some(loader),
            preload: preload.join(":").into(),
        })
    }

    pub(super) fn invocation(&self, image: OsString) -> (OsString, Vec<OsString>) {
        match &self.loader {
            None => (image, Vec::new()),
            Some(loader) => (
                loader.as_os_str().to_owned(),
                vec![
                    "--inhibit-cache".into(),
                    "--inhibit-rpath".into(),
                    OsString::new(),
                    "--preload".into(),
                    self.preload.clone(),
                    image,
                ],
            ),
        }
    }
}
