//! Opens the project, or a file in it, in an installed code editor.
//!
//! Editors are found on `PATH` in a fixed preference order rather than run
//! through a shell, so a path can never be interpreted as a command.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

/// Launchers in preference order: (label, file names to look for).
const EDITORS: &[(&str, &[&str])] = &[
    ("VS Code", &["code.cmd", "code.exe", "code"]),
    ("Cursor", &["cursor.cmd", "cursor.exe", "cursor"]),
    ("Windsurf", &["windsurf.cmd", "windsurf.exe", "windsurf"]),
    ("Zed", &["zed.exe", "zed"]),
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditorError {
    #[error("no supported editor on PATH (VS Code, Cursor, Windsurf or Zed)")]
    NotFound,
    #[error("{0} is not inside this project")]
    Outside(String),
    #[error("could not start {editor}: {message}")]
    Launch {
        editor: &'static str,
        message: String,
    },
}

/// Whether any of `names` is a file in a `PATH` directory.
pub fn on_path(names: &[&str]) -> bool {
    std::env::var_os("PATH").is_some_and(|value| {
        std::env::split_paths(&value).any(|dir| names.iter().any(|name| dir.join(name).is_file()))
    })
}

/// The first installed editor: its label and launcher path.
pub fn find(path_var: Option<OsString>) -> Option<(&'static str, PathBuf)> {
    let dirs: Vec<PathBuf> = path_var
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    EDITORS.iter().find_map(|(label, names)| {
        dirs.iter().find_map(|dir| {
            names
                .iter()
                .map(|name| dir.join(name))
                .find(|candidate| candidate.is_file())
                .map(|found| (*label, found))
        })
    })
}

/// `relative` resolved inside `root`, or `root` itself when absent. Both
/// sides are canonicalized so `..` and links cannot leave the project.
pub fn target(root: &Path, relative: Option<&str>) -> Result<PathBuf, EditorError> {
    let Some(relative) = relative.filter(|value| !value.is_empty()) else {
        return Ok(root.to_path_buf());
    };
    let outside = || EditorError::Outside(relative.to_owned());
    let root = root.canonicalize().map_err(|_| outside())?;
    let path = root.join(relative).canonicalize().map_err(|_| outside())?;
    if path.starts_with(&root) {
        Ok(path)
    } else {
        Err(outside())
    }
}

/// Starts the editor on `target` without waiting for it. Returns its label.
pub fn open(root: &Path, relative: Option<&str>) -> Result<&'static str, EditorError> {
    let target = target(root, relative)?;
    let (label, launcher) = find(std::env::var_os("PATH")).ok_or(EditorError::NotFound)?;
    let mut command = Command::new(&launcher);
    // Editors expect ordinary paths, not Windows' verbatim `\\?\` form.
    let plain = |path: &Path| crate::view::display_path(path);
    // Open the project folder too, so a file lands in its workspace window.
    if target != root {
        command.arg(plain(root));
    }
    command.arg(plain(&target)).current_dir(root);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: `.cmd` launchers would otherwise flash a console.
        command.creation_flags(0x0800_0000);
    }
    command
        .spawn()
        .map(|_| label)
        .map_err(|error| EditorError::Launch {
            editor: label,
            message: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qq-editor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn editors_are_found_in_preference_order_on_path() {
        let first = scratch("first");
        let second = scratch("second");
        std::fs::write(second.join("code.cmd"), "").unwrap();
        std::fs::write(first.join("cursor.cmd"), "").unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        let (label, launcher) = find(Some(path)).unwrap();
        // VS Code wins over Cursor even when Cursor's directory comes first.
        assert_eq!(label, "VS Code");
        assert_eq!(launcher, second.join("code.cmd"));
        assert_eq!(find(Some(OsString::new())), None);
        assert_eq!(find(None), None);
    }

    #[test]
    fn targets_stay_inside_the_project() {
        let root = scratch("root");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "").unwrap();
        assert_eq!(target(&root, None).unwrap(), root);
        assert_eq!(target(&root, Some("")).unwrap(), root);
        assert!(
            target(&root, Some("src/main.rs"))
                .unwrap()
                .ends_with("main.rs")
        );
        assert_eq!(
            target(&root, Some("../elsewhere")),
            Err(EditorError::Outside("../elsewhere".to_owned()))
        );
        assert_eq!(
            target(&root, Some("missing.rs")),
            Err(EditorError::Outside("missing.rs".to_owned()))
        );
    }
}
