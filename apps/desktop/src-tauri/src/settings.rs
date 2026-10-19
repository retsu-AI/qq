//! Desktop-only preferences: recently opened projects and the last chosen
//! model. Everything about sessions lives in the QQ server, not here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

const FILE_NAME: &str = "settings.json";
const MAX_RECENT_PROJECTS: usize = 12;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub recent_projects: Vec<PathBuf>,
    pub preferred_model: Option<String>,
}

impl Settings {
    pub fn load(app: &AppHandle) -> Self {
        let mut settings: Self = settings_path(app)
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        settings.dedupe_recents();
        settings
    }

    /// Best effort: a failed write loses a preference, never work.
    pub fn save(&self, app: &AppHandle) {
        let Some(path) = settings_path(app) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, bytes);
        }
    }

    pub fn remember_project(&mut self, path: &Path) {
        let key = recent_key(path);
        self.recent_projects
            .retain(|existing| recent_key(existing) != key);
        self.recent_projects.insert(0, path.to_path_buf());
        self.recent_projects.truncate(MAX_RECENT_PROJECTS);
    }

    /// Older builds stored paths without Windows' verbatim `\\?\` prefix,
    /// so one folder could be listed twice. Keeps the first (newest) entry.
    fn dedupe_recents(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.recent_projects
            .retain(|path| seen.insert(recent_key(path)));
    }
}

/// One folder's identity in the recent list: the verbatim prefix removed,
/// separators unified and, on Windows, case folded.
fn recent_key(path: &Path) -> String {
    let text = crate::view::display_path(path).replace('/', "\\");
    let text = text.trim_end_matches('\\').to_owned();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_and_plain_forms_of_one_folder_are_one_recent() {
        let mut settings = Settings {
            recent_projects: vec![
                PathBuf::from(r"\\?\C:\work\qq"),
                PathBuf::from(r"\\?\C:\work\game"),
                PathBuf::from(r"C:\work\qq"),
                PathBuf::from(r"C:\work\game\"),
            ],
            preferred_model: None,
        };
        settings.dedupe_recents();
        assert_eq!(
            settings.recent_projects,
            vec![
                PathBuf::from(r"\\?\C:\work\qq"),
                PathBuf::from(r"\\?\C:\work\game")
            ]
        );
        settings.remember_project(Path::new(r"\\?\C:\work\game"));
        settings.remember_project(Path::new(r"C:\Work\QQ"));
        assert_eq!(settings.recent_projects.len(), 2);
        assert_eq!(settings.recent_projects[0], PathBuf::from(r"C:\Work\QQ"));
    }
}
