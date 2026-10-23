//! Preserve the task directory when launching Windows command interpreters.
use std::{io, path::Path};

/// cmd interprets verbatim drive paths as UNC and silently changes directories.
/// Simplify only when DOS path rules preserve the same name; never strip the
/// prefix from reserved names, trailing dots/spaces, or extended-only paths.
pub(super) fn working_directory<'a>(program: &Path, cwd: &'a Path) -> io::Result<&'a Path> {
    if !program
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe"))
    {
        return Ok(cwd);
    }
    let simplified = dunce::simplified(cwd);
    if !simplified.is_absolute()
        || !matches!(
            simplified.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::Disk(_))
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cmd cannot retain this working directory; use exec with a program that supports extended paths",
        ));
    }
    Ok(simplified)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_directory_preserves_identity_or_refuses_extended_only_paths() {
        let shell = Path::new(r"C:\Windows\System32\CMD.EXE");
        let cwd = Path::new(r"\\?\C:\task with spaces\日本語");
        assert_eq!(
            working_directory(shell, cwd).unwrap(),
            Path::new(r"C:\task with spaces\日本語")
        );
        assert_eq!(working_directory(Path::new("tool.exe"), cwd).unwrap(), cwd);
        let long = std::path::PathBuf::from(format!(r"\\?\C:\{}", "a".repeat(300)));
        assert_eq!(
            working_directory(shell, &long).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        for unsupported in [
            r"\\?\C:\task.",
            r"\\?\C:\task ",
            r"\\?\C:\NUL\task",
            r"\\?\UNC\server\share\task",
            r"\\server\share\task",
            r"C:relative",
        ] {
            assert_eq!(
                working_directory(shell, Path::new(unsupported))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Unsupported
            );
        }
    }
}
