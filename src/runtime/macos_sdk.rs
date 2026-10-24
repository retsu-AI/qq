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
    discover_selected(
        &[Path::new("/opt/homebrew"), Path::new("/usr/local")],
        &selected,
        control,
        |image, deadline| {
            let mut command = std::process::Command::new("/usr/bin/otool");
            command
                .arg("-L")
                .arg(image)
                .current_dir(control)
                .env_clear()
                .env("PATH", "/usr/bin:/bin");
            super::unix_sdk::query_until(command, deadline)
        },
    )
}

fn discover_selected(
    prefixes: &[&Path],
    selected: &Path,
    control: &Path,
    metadata: impl FnMut(&Path, Instant) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<Inputs> {
    for prefix in prefixes {
        let image = prefix.join("bin/node");
        if !image.is_file() || image.canonicalize()? != selected {
            continue;
        }
        if !selected.starts_with(prefix.join("Cellar")) {
            continue;
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        return inputs(prefix, selected, control, deadline, metadata);
    }
    Ok(Inputs::default())
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "installed Node input escapes its conventional SDK root",
    )
}

fn denied_input(stage: &str, input: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("installed Node {stage}: {}", input.display()),
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
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "installed Node metadata requires a canonical private control directory",
        ));
    }
    let cellar = prefix.join("Cellar");
    let suffix = image
        .strip_prefix(&cellar)
        .map_err(|_| denied_input("image is outside its Cellar", image))?;
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
        return Err(denied_input(
            "image has a redirected or unsupported formula layout",
            image,
        ));
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
                return Err(denied_input(
                    "runtime root is redirected or overlaps control",
                    &root,
                ));
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
            return Err(denied_input(
                "npm package escapes its conventional root",
                &npm,
            ));
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
            } else if let Some(relative) = name.strip_prefix("@rpath/") {
                let relative = Path::new(relative);
                if relative.as_os_str().is_empty()
                    || relative
                        .components()
                        .any(|component| !matches!(component, std::path::Component::Normal(_)))
                {
                    return Err(denied_input(
                        "library run-path suffix is not normal",
                        relative,
                    ));
                }
                // Homebrew's Node imports libnode through @rpath. Inspect only
                // files inside its already-admitted formula lib or the current
                // admitted library's directory, never environment search paths.
                let mut found = None;
                for root in [
                    version.join("lib"),
                    image.parent().ok_or_else(denied)?.to_owned(),
                ] {
                    if root == bin {
                        continue;
                    }
                    let candidate = root.join(relative);
                    if candidate.try_exists()? {
                        let canonical = candidate.canonicalize()?;
                        if !canonical.starts_with(&root) {
                            return Err(denied_input(
                                "library run-path alias escapes its root",
                                &candidate,
                            ));
                        }
                        found = Some(canonical);
                        break;
                    }
                }
                found.ok_or_else(|| {
                    denied_input("library run-path input is missing", Path::new(name))
                })?
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
                return Err(denied_input(
                    "library install name is unsupported",
                    &dependency,
                ));
            }
            let dependency = dependency.canonicalize()?;
            if !dependency.starts_with(&cellar) || !dependency.is_file() {
                return Err(denied_input("library escapes its Cellar", &dependency));
            }
            let root = dependency.parent().ok_or_else(denied)?.to_owned();
            if root.starts_with(control) || control.starts_with(&root) {
                return Err(denied_input("library root overlaps control", &root));
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
    if let Some(root) = roots
        .iter()
        .find(|root| root.starts_with(control) || control.starts_with(root))
    {
        return Err(denied_input("runtime root overlaps control", root));
    }
    Ok(Inputs {
        executable_directories: vec![bin],
        read_only: roots.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_node_installations_do_not_prevent_runtime_startup() {
        let fixture = tempfile::tempdir().unwrap();
        let prefix = fixture.path().canonicalize().unwrap();
        std::fs::create_dir(prefix.join("bin")).unwrap();
        std::fs::create_dir(prefix.join("control")).unwrap();
        let image = prefix.join("bin/node");
        std::fs::write(&image, b"standalone Node").unwrap();
        let inputs = discover_selected(&[&prefix], &image, &prefix.join("control"), |_, _| {
            panic!("unsupported installations must not be inspected")
        })
        .unwrap();
        assert!(inputs.executable_directories.is_empty());
        assert!(inputs.read_only.is_empty());
    }
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
        std::fs::create_dir(prefix.join("bin")).unwrap();
        std::os::unix::fs::symlink(&image, prefix.join("bin/node")).unwrap();
        let result = discover_selected(&[&prefix], &image, &prefix.join("control"), |path, _| {
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
    fn unsupported_library_errors_identify_the_actual_install_name() {
        let (_fixture, prefix, image, _) = fixture();
        let name = "@unsupported/libnode.127.dylib";
        let error = inputs(
            &prefix,
            &image,
            &prefix.join("control"),
            Instant::now() + std::time::Duration::from_secs(5),
            |_, _| {
                Ok(Some(
                    format!(
                        "node:\n\t{name} (compatibility version 1.0.0, current version 1.0.0)\n"
                    )
                    .into_bytes(),
                ))
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error
                .to_string()
                .contains("library install name is unsupported")
        );
        assert!(error.to_string().contains(name));
    }

    #[test]
    fn formula_rpath_resolves_libnode_and_its_self_install_name() {
        let (_fixture, prefix, image, library) = fixture();
        let libnode = image
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("lib/libnode.127.dylib");
        std::fs::write(&libnode, b"libnode").unwrap();
        let mut queried = Vec::new();
        let result = inputs(
            &prefix,
            &image,
            &prefix.join("control"),
            Instant::now() + std::time::Duration::from_secs(5),
            |current, _| {
                queried.push(current.to_owned());
                let names = if current == image {
                    "@rpath/libnode.127.dylib".to_owned()
                } else if current == libnode {
                    format!("@rpath/libnode.127.dylib\n{}", library.display())
                } else {
                    library.display().to_string()
                };
                Ok(Some(
                    format!(
                        "{}:\n{}",
                        current.display(),
                        names
                            .lines()
                            .map(|name| format!(
                                "\t{name} (compatibility version 1.0.0, current version 1.0.0)\n"
                            ))
                            .collect::<String>()
                    )
                    .into_bytes(),
                ))
            },
        )
        .unwrap();
        assert_eq!(queried.len(), 3);
        assert!(queried.contains(&libnode));
        assert_eq!(result.read_only.len(), 3);
        assert!(!result.read_only.contains(&prefix.join("Cellar")));
    }

    #[test]
    fn formula_rpath_refuses_missing_escaping_and_non_normal_inputs() {
        let (_fixture, prefix, image, _) = fixture();
        let private = prefix.join("private.dylib");
        std::fs::write(&private, b"private").unwrap();
        let lib = image.parent().unwrap().parent().unwrap().join("lib");
        std::os::unix::fs::symlink(&private, lib.join("escape.dylib")).unwrap();
        for name in [
            "@rpath/missing.dylib",
            "@rpath/escape.dylib",
            "@rpath/../../private.dylib",
            "@rpath//usr/lib/private.dylib",
        ] {
            let result = inputs(
                &prefix,
                &image,
                &prefix.join("control"),
                Instant::now() + std::time::Duration::from_secs(5),
                |_, _| {
                    Ok(Some(
                        format!(
                            "node:\n\t{name} (compatibility version 1.0.0, current version 1.0.0)\n"
                        )
                        .into_bytes(),
                    ))
                },
            );
            assert!(result.is_err(), "must reject {name}");
        }
        assert_eq!(std::fs::read(private).unwrap(), b"private");
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
