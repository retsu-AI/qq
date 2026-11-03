use std::{io::Read, path::Path};

use qq_script::{Language, Module};

use crate::{plan::SourceFingerprint, workspace::Workspace};

pub(crate) const WORKSPACE_MODULE_DIRECTORY: &str = ".qq/code";
pub(crate) const GLOBAL_MODULE_DIRECTORY: &str = "code";
const MAX_MODULES: usize = 32;
const MAX_MODULE_BYTES: u64 = 64 * 1024;
const MAX_TOTAL_MODULE_BYTES: usize = 256 * 1024;

pub(crate) fn load_blocking(
    language: Language,
    workspace: &Workspace,
    global: Option<&Path>,
) -> (Vec<Module>, Vec<SourceFingerprint>) {
    let mut modules: Vec<Module> = Vec::new();
    let mut fingerprints = Vec::new();
    let mut total = 0_usize;
    let workspace_directory = workspace.path().join(WORKSPACE_MODULE_DIRECTORY);
    let global_directory = global.map(|dir| dir.join(GLOBAL_MODULE_DIRECTORY));
    for directory in std::iter::once(workspace_directory).chain(global_directory) {
        let fingerprint = SourceFingerprint::capture(directory.clone());
        let present = fingerprint.is_present();
        fingerprints.push(fingerprint);
        if !present {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(&directory) else {
            continue;
        };
        let Ok(opened) = Workspace::open(&canonical) else {
            continue;
        };
        let Ok(listing) = opened.root().read_dir(".") else {
            continue;
        };
        let mut names: Vec<String> = listing
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter_map(|entry| {
                let file_name = entry.file_name().into_string().ok()?;
                let stem = file_name.strip_suffix(&format!(".{}", language.module_extension()))?;
                language.module_name_allowed(stem).then(|| stem.to_owned())
            })
            .collect();
        names.sort();
        for name in names {
            if modules.len() >= MAX_MODULES || modules.iter().any(|module| module.name == name) {
                continue;
            }
            let file = format!("{name}.{}", language.module_extension());
            fingerprints.push(SourceFingerprint::capture(canonical.join(&file)));
            let Some(source) = read_module(&opened, &file) else {
                continue;
            };
            if total + source.len() > MAX_TOTAL_MODULE_BYTES {
                continue;
            }
            total += source.len();
            modules.push(Module { name, source });
        }
    }
    modules.sort_by(|a, b| a.name.cmp(&b.name));
    (modules, fingerprints)
}

fn read_module(directory: &Workspace, file: &str) -> Option<String> {
    let mut source = String::new();
    directory
        .root()
        .open(file)
        .ok()?
        .take(MAX_MODULE_BYTES + 1)
        .read_to_string(&mut source)
        .ok()?;
    (source.len() as u64 <= MAX_MODULE_BYTES).then_some(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(path: &Path) -> Workspace {
        Workspace::open(&std::fs::canonicalize(path).unwrap()).unwrap()
    }

    #[test]
    fn workspace_modules_shadow_global_ones_and_bad_names_are_skipped() {
        let workspace = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let local = workspace.path().join(WORKSPACE_MODULE_DIRECTORY);
        let shared = global.path().join(GLOBAL_MODULE_DIRECTORY);
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(local.join("repo.py"), "X = 'local'\n").unwrap();
        std::fs::write(shared.join("repo.py"), "X = 'global'\n").unwrap();
        std::fs::write(shared.join("util.py"), "Y = 1\n").unwrap();
        std::fs::write(local.join("asyncio.py"), "").unwrap();
        std::fs::write(local.join("bad-name.py"), "").unwrap();
        std::fs::write(local.join("notes.txt"), "").unwrap();
        std::fs::write(
            local.join("huge.py"),
            "x".repeat(MAX_MODULE_BYTES as usize + 1),
        )
        .unwrap();

        let (modules, fingerprints) = load_blocking(
            Language::Python,
            &open(workspace.path()),
            Some(global.path()),
        );

        let found: Vec<(&str, &str)> = modules
            .iter()
            .map(|module| (module.name.as_str(), module.source.as_str()))
            .collect();
        assert_eq!(found, [("repo", "X = 'local'\n"), ("util", "Y = 1\n")]);
        assert!(fingerprints.iter().all(SourceFingerprint::is_current));
    }

    #[test]
    fn missing_directories_load_nothing_but_are_watched() {
        let workspace = tempfile::tempdir().unwrap();
        let (modules, fingerprints) =
            load_blocking(Language::Python, &open(workspace.path()), None);
        assert!(modules.is_empty());
        assert_eq!(fingerprints.len(), 1);
        assert!(!fingerprints[0].is_present());
    }
}
