//! Read-only inputs for the server's conventionally installed Homebrew Node.
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Default)]
pub(super) struct Inputs {
    pub(super) executable_directories: Vec<PathBuf>,
    pub(super) read_only: Vec<PathBuf>,
}

#[cfg(target_os = "macos")]
pub(super) fn discover(control: &Path) -> io::Result<Inputs> {
    let Some(path) = std::env::var_os("PATH") else {
        return Ok(Inputs::default());
    };
    let Some(selected) = std::env::split_paths(&path)
        .map(|directory| directory.join("node"))
        .find(|image| image.is_file())
    else {
        return Ok(Inputs::default());
    };
    let selected = selected.canonicalize()?;
    for prefix in [Path::new("/opt/homebrew"), Path::new("/usr/local")] {
        let image = prefix.join("bin/node");
        if !image.is_file() || image.canonicalize()? != selected {
            continue;
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        return inputs(prefix, &selected, control, deadline, |image, deadline| {
            let mut command = std::process::Command::new("/usr/bin/otool");
            command
                .arg("-L")
                .arg(image)
                .current_dir(control)
                .env_clear()
                .env("PATH", "/usr/bin:/bin");
            super::unix_sdk::query_until(command, deadline)
        });
    }
    Ok(Inputs::default())
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "installed Node input escapes its conventional SDK root",
    )
}

fn inputs(
    prefix: &Path,
    image: &Path,
    control: &Path,
    deadline: Instant,
    mut metadata: impl FnMut(&Path, Instant) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<Inputs> {
    if !control.is_absolute() || control.canonicalize()? != control {
        return Err(denied());
    }
    let cellar = prefix.join("Cellar");
    let suffix = image.strip_prefix(&cellar).map_err(|_| denied())?;
    let components = suffix.iter().collect::<Vec<_>>();
    if components.len() != 4
        || components[2] != "bin"
        || components[3] != "node"
        || !components[0].to_str().is_some_and(|name| {
            name == "node"
                || name.strip_prefix("node@").is_some_and(|version| {
                    !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                })
        })
        || prefix.canonicalize()? != prefix
        || cellar.canonicalize()? != cellar
        || image.canonicalize()? != image
        || !image.is_file()
    {
        return Err(denied());
    }
    let bin = image.parent().ok_or_else(denied)?.to_owned();
    let version = bin.parent().ok_or_else(denied)?;
    let mut roots = BTreeSet::new();
    for root in [bin.clone(), version.join("lib")] {
        if root.exists() {
            if root.canonicalize()? != root
                || root.starts_with(control)
                || control.starts_with(&root)
            {
                return Err(denied());
            }
            roots.insert(root);
        }
    }
    // npm may be linked separately from the selected Node formula. Admit only
    // its conventional package tree, never all of Homebrew's lib or bin.
    let npm = prefix.join("lib/node_modules/npm");
    if npm.is_dir() {
        let npm = npm.canonicalize()?;
        if !npm.starts_with(&cellar) && npm != prefix.join("lib/node_modules/npm") {
            return Err(denied());
        }
        roots.insert(npm);
    }
    let mut seen = BTreeSet::new();
    let mut pending = vec![image.to_owned()];
    while let Some(image) = pending.pop() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Node SDK metadata deadline elapsed",
            ));
        }
        if !seen.insert(image.clone()) {
            continue;
        }
        if seen.len() > 32 {
            return Err(io::Error::other("too many Node SDK library inputs"));
        }
        let output = metadata(&image, deadline)?
            .ok_or_else(|| io::Error::other("installed Node dependency metadata unavailable"))?;
        let output = std::str::from_utf8(&output)
            .map_err(|_| io::Error::other("invalid Node dependency metadata"))?;
        for line in output.lines().filter(|line| line.starts_with('\t')) {
            let (name, _) = line
                .trim()
                .split_once(" (compatibility version ")
                .ok_or_else(|| io::Error::other("invalid Node dependency entry"))?;
            let dependency = if let Some(relative) = name.strip_prefix("@loader_path/") {
                image.parent().ok_or_else(denied)?.join(relative)
            } else if let Some(relative) = name.strip_prefix("@executable_path/") {
                bin.join(relative)
            } else {
                PathBuf::from(name)
            };
            if dependency.is_absolute()
                && !dependency
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                && (dependency.starts_with("/usr/lib") || dependency.starts_with("/System/Library"))
            {
                continue;
            }
            if !dependency.is_absolute() {
                return Err(denied());
            }
            let dependency = dependency.canonicalize()?;
            if !dependency.starts_with(&cellar) || !dependency.is_file() {
                return Err(denied());
            }
            let root = dependency.parent().ok_or_else(denied)?.to_owned();
            if root.starts_with(control) || control.starts_with(&root) {
                return Err(denied());
            }
            roots.insert(root);
            if !seen.contains(&dependency) && !pending.contains(&dependency) {
                if seen.len() + pending.len() >= 32 {
                    return Err(io::Error::other("too many Node SDK library inputs"));
                }
                pending.push(dependency);
            }
        }
    }
    if roots
        .iter()
        .any(|root| root.starts_with(control) || control.starts_with(root))
    {
        return Err(denied());
    }
    Ok(Inputs {
        executable_directories: vec![bin],
        read_only: roots.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let fixture = tempfile::tempdir().unwrap();
        let prefix = fixture.path().canonicalize().unwrap();
        std::fs::create_dir(prefix.join("control")).unwrap();
        let image = prefix.join("Cellar/node@22/22.23.2/bin/node");
        let library = prefix.join("Cellar/icu4c/77/lib/libicu.dylib");
        for path in [&image, &library] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"native image").unwrap();
        }
        std::fs::create_dir_all(prefix.join("Cellar/node@22/22.23.2/lib")).unwrap();
        (fixture, prefix, image, library)
    }
    #[test]
    fn selected_node_admits_only_its_concrete_runtime_inputs() {
        let (_fixture, prefix, image, library) = fixture();
        let result = inputs(&prefix, &image, &prefix.join("control"), Instant::now() + std::time::Duration::from_secs(5), |path, _| {
            Ok(Some(if path == image { format!("node:\n\t{} (compatibility version 1.0.0, current version 1.0.0)\n\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n", library.display()).into_bytes() } else { b"library:\n".to_vec() }))
        }).unwrap();
        assert_eq!(result.executable_directories, [image.parent().unwrap()]);
        assert_eq!(result.read_only.len(), 3);
        assert!(
            result
                .read_only
                .contains(&library.parent().unwrap().to_owned())
        );
        assert!(!result.read_only.contains(&prefix));
    }
    #[test]
    fn dependencies_cannot_redirect_into_private_directories() {
        let (_fixture, prefix, image, library) = fixture();
        let private = prefix.join("private");
        std::fs::create_dir(&private).unwrap();
        let secret = private.join("credential");
        std::fs::write(&secret, b"private").unwrap();
        std::fs::remove_file(&library).unwrap();
        std::os::unix::fs::symlink(&secret, &library).unwrap();
        let result = inputs(
            &prefix,
            &image,
            &prefix.join("control"),
            Instant::now() + std::time::Duration::from_secs(5),
            |_, _| {
                Ok(Some(
                    format!(
                        "node:\n\t{} (compatibility version 1.0.0, current version 1.0.0)\n",
                        library.display()
                    )
                    .into_bytes(),
                ))
            },
        );
        assert_eq!(
            result.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(&secret).unwrap(), b"private");
    }

    #[test]
    fn dependencies_cannot_admit_control_storage_beneath_the_cellar() {
        let (_fixture, prefix, image, _) = fixture();
        let control = prefix.join("Cellar/control");
        std::fs::create_dir(&control).unwrap();
        let private = control.join("credential.dylib");
        std::fs::write(&private, b"private").unwrap();
        let result = inputs(
            &prefix,
            &image,
            &control,
            Instant::now() + std::time::Duration::from_secs(5),
            |_, _| {
                Ok(Some(
                    format!(
                        "node:\n\t{} (compatibility version 1.0.0, current version 1.0.0)\n",
                        private.display()
                    )
                    .into_bytes(),
                ))
            },
        );
        assert_eq!(
            result.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(private).unwrap(), b"private");
    }

    #[test]
    fn npm_package_aliases_cannot_admit_private_files() {
        let (_fixture, prefix, image, _) = fixture();
        let private = prefix.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("credential"), b"private").unwrap();
        let modules = prefix.join("lib/node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        std::os::unix::fs::symlink(&private, modules.join("npm")).unwrap();
        let result = inputs(
            &prefix,
            &image,
            &prefix.join("control"),
            Instant::now() + std::time::Duration::from_secs(5),
            |_, _| panic!("npm alias must be refused before querying metadata"),
        );
        assert_eq!(
            result.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read(private.join("credential")).unwrap(),
            b"private"
        );
    }

    #[test]
    fn control_aliases_and_relative_paths_do_not_query_metadata() {
        let (_fixture, prefix, image, _) = fixture();
        let alias = prefix.join("control-alias");
        std::os::unix::fs::symlink(prefix.join("control"), &alias).unwrap();
        for control in [Path::new("relative-control"), alias.as_path()] {
            let result = inputs(
                &prefix,
                &image,
                control,
                Instant::now() + std::time::Duration::from_secs(5),
                |_, _| panic!("control aliases must be refused before querying metadata"),
            );
            assert_eq!(
                result.err().unwrap().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn control_overlap_and_expired_deadlines_do_not_query_metadata() {
        let (_fixture, prefix, image, _) = fixture();
        for (control, deadline) in [
            (
                image.parent().unwrap().to_owned(),
                Instant::now() + std::time::Duration::from_secs(5),
            ),
            (prefix.join("control"), Instant::now()),
        ] {
            assert!(
                inputs(&prefix, &image, &control, deadline, |_, _| panic!(
                    "inadmissible SDK inputs must not query metadata"
                ))
                .is_err()
            );
        }
    }
}
