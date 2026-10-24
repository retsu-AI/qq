#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {}
#[cfg(windows)]
fn main() -> std::io::Result<()> {
    probe::main()
}
#[cfg(windows)]
mod probe {
    use std::{
        collections::BTreeSet,
        fs,
        io::{self, Read as _},
        path::{Path, PathBuf},
    };
    fn invalid() -> io::Error {
        io::Error::other("invalid or oversized PE metadata")
    }
    fn u16_at(bytes: &[u8], at: usize) -> io::Result<u16> {
        Ok(u16::from_le_bytes(
            bytes
                .get(at..at + 2)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        ))
    }
    fn u32_at(bytes: &[u8], at: usize) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            bytes
                .get(at..at + 4)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        ))
    }
    fn imports(path: &Path) -> io::Result<Vec<String>> {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(128 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 128 * 1024 * 1024 || bytes.get(..2) != Some(b"MZ") {
            return Err(invalid());
        }
        let pe = u32_at(&bytes, 0x3c)? as usize;
        if bytes.get(pe..pe + 4) != Some(b"PE\0\0") {
            return Err(invalid());
        }
        let count = u16_at(&bytes, pe + 6)? as usize;
        if count > 96 {
            return Err(invalid());
        }
        let optional = pe + 24;
        let directories = match u16_at(&bytes, optional)? {
            0x20b => optional + 112,
            0x10b => optional + 96,
            _ => return Err(invalid()),
        };
        let sections = optional + u16_at(&bytes, pe + 20)? as usize;
        let resolve = |rva: u32| -> io::Result<usize> {
            for n in 0..count {
                let section = sections + n * 40;
                let virtual_address = u32_at(&bytes, section + 12)?;
                let size = u32_at(&bytes, section + 16)?;
                if rva >= virtual_address && rva - virtual_address < size {
                    let offset =
                        (rva - virtual_address) as usize + u32_at(&bytes, section + 20)? as usize;
                    if offset < bytes.len() {
                        return Ok(offset);
                    }
                }
            }
            Err(invalid())
        };
        let import_rva = u32_at(&bytes, directories + 8)?;
        if import_rva == 0 {
            return Ok(Vec::new());
        }
        let start = resolve(import_rva)?;
        let mut names = BTreeSet::new();
        for n in 0..256 {
            let descriptor = start + n * 20;
            let name = u32_at(&bytes, descriptor + 12)?;
            if name == 0 {
                return Ok(names.into_iter().collect());
            }
            let offset = resolve(name)?;
            let text = bytes
                .get(offset..)
                .ok_or_else(invalid)?
                .iter()
                .take(256)
                .position(|b| *b == 0)
                .ok_or_else(invalid)?;
            let name = std::str::from_utf8(&bytes[offset..offset + text]).map_err(|_| invalid())?;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err(invalid());
            }
            names.insert(name.to_owned());
        }
        Err(invalid())
    }
    pub fn main() -> io::Result<()> {
        let args = std::env::args_os().skip(1).collect::<Vec<_>>();
        if args.first().is_some_and(|s| s == "--prepare") && args.len() == 3 {
            let cargo = PathBuf::from(&args[1]).canonicalize()?;
            let rustc = PathBuf::from(&args[2]).canonicalize()?;
            if cargo.parent() != rustc.parent() {
                return Err(invalid());
            }
            let bin = cargo.parent().ok_or_else(invalid)?;
            let lib = bin.parent().ok_or_else(invalid)?.join("lib");
            let system = PathBuf::from(std::env::var_os("SYSTEMROOT").ok_or_else(invalid)?)
                .join("System32")
                .canonicalize()?;
            let mut pending = std::collections::VecDeque::from([cargo.clone(), rustc.clone()]);
            let mut images = BTreeSet::new();
            let mut rows = Vec::new();
            while let Some(image) = pending.pop_front() {
                if !images.insert(image.clone()) {
                    continue;
                }
                if images.len() > 128 {
                    images.remove(&image);
                    rows.push(serde_json::json!({"metadata_incomplete":"128 image bound reached"}));
                    break;
                }
                let names = match imports(&image) {
                    Ok(names) => names,
                    Err(error) => {
                        rows.push(serde_json::json!({"image":image,"metadata_incomplete":error.to_string(),"file_bytes":fs::metadata(&image).ok().map(|m|m.len())}));
                        continue;
                    }
                };
                for name in &names {
                    let mut resolved = [
                        image.parent().ok_or_else(invalid)?,
                        bin,
                        lib.as_path(),
                        system.as_path(),
                    ]
                    .into_iter()
                    .map(|dir| dir.join(name))
                    .find(|path| path.is_file());
                    if let Some(path) = resolved.take() {
                        match path.canonicalize() {
                            Ok(path) => { pending.push_back(path.clone()); resolved=Some(path); }
                            Err(error) => rows.push(serde_json::json!({"image":path,"metadata_incomplete":error.to_string()})),
                        }
                    }
                    rows.push(serde_json::json!({"image":image,"import":name,"resolved":resolved}));
                }
            }
            let mut manifest = serde_json::json!({"cargo":cargo,"rustc":rustc,"system32":system,"images":images,"imports":rows});
            if serde_json::to_vec(&manifest)?.len() > 1024 * 1024 {
                manifest["imports"] =
                    serde_json::json!([{"metadata_incomplete":"1MiB manifest bound reached"}]);
            }
            println!("{manifest}");
            return Ok(());
        }
        let load_only = args.first().is_some_and(|arg| arg == "--load");
        if args.len() != if load_only { 2 } else { 1 } {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        fs::File::open(&args[usize::from(load_only)])?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(invalid());
        }
        let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
        let state = rappct::token::query_current_process_token().map_err(io::Error::other)?;
        let token = firehazard::open_process_token(
            firehazard::get_current_process(),
            firehazard::token::QUERY,
        )
        .map_err(io::Error::from)?;
        println!(
            "actual image-probe token: appcontainer={} lpac_telemetry={} restrictors={} integrity={:?} privileges={:?}",
            state.is_appcontainer,
            state.is_lpac,
            token
                .restricted_sids()
                .map_err(io::Error::from)?
                .group_count(),
            token.integrity_level().map_err(io::Error::from)?,
            token.privileges().map_err(io::Error::from)?.privileges()
        );
        // The pinned safe wrapper panics on a documented null default DACL.
        // Keep that optional metadata limitation from hiding actual Cargo.
        match std::panic::catch_unwind(|| {
            token
                .default_dacl()
                .map(|dacl| format!("{:?}", dacl.default_dacl().aces().collect::<Vec<_>>()))
        }) {
            Ok(Ok(dacl)) => println!("actual payload default DACL: {dacl}"),
            Ok(Err(error)) => println!("actual payload default DACL query incomplete: {error:?}"),
            Err(_) => println!(
                "actual payload default DACL query incomplete: pinned wrapper rejected null/invalid data"
            ),
        }
        let name = format!("Local\\qq-crypto-self-event-{}", std::process::id());
        match winsafe::HEVENT::CreateEvent(None, false, false, Some(&name)) {
            Ok(event) => {
                println!("self-event default-DACL create succeeded: {name}");
                let reopen = event
                    .OpenEvent(
                        winsafe::co::EVENT_RIGHTS::MODIFY_STATE
                            | winsafe::co::EVENT_RIGHTS::SYNCHRONIZE,
                        false,
                        &name,
                    )
                    .map(|_| ());
                println!("self-event default-DACL write/synchronize reopen: {reopen:?}");
            }
            Err(error) => println!("self-event default-DACL create: {error:?}"),
        }
        if load_only {
            let system = PathBuf::from(manifest["system32"].as_str().ok_or_else(invalid)?);
            if !system.is_absolute() {
                return Err(invalid());
            }
            let cargo = manifest["cargo"].as_str().ok_or_else(invalid)?;
            let rows = manifest["imports"].as_array().ok_or_else(invalid)?;
            if rows.len() > 32768 {
                return Err(invalid());
            }
            let mut loaded = BTreeSet::new();
            for row in rows {
                if row["image"].as_str() != Some(cargo) {
                    continue;
                }
                let Some(path) = row["resolved"].as_str() else {
                    continue;
                };
                let path = PathBuf::from(path);
                if path.parent() == Some(system.as_path()) {
                    loaded.insert(path);
                }
            }
            let mut loaded = loaded.into_iter().collect::<Vec<_>>();
            loaded.sort_by_key(|path| {
                !path
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("user32.dll"))
            });
            println!(
                "Cargo DLL initialization metadata: selected={} USER32_present={} (empty/missing USER32 means incomplete attribution)",
                loaded.len(),
                loaded.iter().any(|path| path
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("user32.dll")))
            );
            for path in loaded {
                use std::io::Write as _;
                println!(
                    "exact-Cargo-import LoadLibraryW begin path={}",
                    path.display()
                );
                io::stdout().flush()?;
                let result =
                    winsafe::HINSTANCE::LoadLibrary(path.to_str().ok_or_else(invalid)?).map(|_| ());
                println!(
                    "exact-Cargo-import LoadLibraryW path={} initialization={result:?}",
                    path.display()
                );
                io::stdout().flush()?;
            }
            return Ok(());
        }
        let images = manifest["images"].as_array().ok_or_else(invalid)?;
        if images.len() > 128 {
            return Err(invalid());
        }
        for image in images {
            let path = image.as_str().ok_or_else(invalid)?;
            for (name, rights) in [
                ("READ_DATA", 1),
                ("EXECUTE", 0x20),
                ("READ_ATTRIBUTES", 0x80),
            ] {
                use std::os::windows::fs::OpenOptionsExt as _;
                let result = fs::File::options()
                    .access_mode(rights)
                    .share_mode(7)
                    .open(path)
                    .map(|_| ())
                    .map_err(|e| (e.raw_os_error(), e.kind()));
                println!("exact-image path={path} access={name} result={result:?}");
            }
            let mapped = winsafe::HFILE::CreateFile(
                path,
                winsafe::co::GENERIC::READ,
                Some(
                    winsafe::co::FILE_SHARE::READ
                        | winsafe::co::FILE_SHARE::WRITE
                        | winsafe::co::FILE_SHARE::DELETE,
                ),
                None,
                winsafe::co::DISPOSITION::OPEN_EXISTING,
                winsafe::co::FILE_ATTRIBUTE::NORMAL,
                None,
                None,
                None,
            )
            .and_then(|(file, _)| {
                file.CreateFileMapping(
                    None,
                    winsafe::co::PAGE::READONLY,
                    Some(winsafe::co::SEC::IMAGE),
                    None,
                    None,
                )
                .map(|_| ())
            });
            println!("exact-image path={path} SEC_IMAGE={mapped:?}");
        }
        Ok(())
    }
}
