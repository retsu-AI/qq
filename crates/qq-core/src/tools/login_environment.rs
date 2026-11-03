#[cfg(all(test, unix))]
mod tests;

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::SystemTime,
};

use tokio::sync::Semaphore;

pub type Environment = Arc<[(OsString, OsString)]>;

#[derive(Debug)]
pub struct LoginEnvironment {
    capture: Arc<Semaphore>,
    generation: AtomicU64,
    warned: AtomicBool,
    cached: Mutex<Option<Cached>>,
    #[cfg(test)]
    source: Option<Source>,

    #[cfg(all(test, unix))]
    inspection: Mutex<Option<tests::InspectionGate>>,
}

#[derive(Debug, Clone)]
struct Cached {
    generation: u64,
    source: Source,
    files: Vec<(PathBuf, Option<SystemTime>)>,
    environment: Option<Environment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Source {
    pub(super) shell: OsString,
    pub(super) home: Option<PathBuf>,
    pub(super) startup: Vec<PathBuf>,
}

impl Default for LoginEnvironment {
    fn default() -> Self {
        Self {
            capture: Arc::new(Semaphore::new(1)),
            generation: AtomicU64::new(0),
            warned: AtomicBool::new(false),
            cached: Mutex::new(None),
            #[cfg(test)]
            source: None,

            #[cfg(all(test, unix))]
            inspection: Mutex::new(None),
        }
    }
}

impl LoginEnvironment {
    #[cfg(all(test, unix))]
    pub(super) fn for_source(source: Source) -> Self {
        Self {
            source: Some(source),
            ..Self::default()
        }
    }
    pub fn reload(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) async fn get(
        &self,
        workspace: &crate::workspace::Workspace,
        producer: Arc<crate::ProcessProducer>,
        cancelled: &super::dispatch::ToolCancellation,
        process: &mut super::ToolTaskLease,
    ) -> (Option<Environment>, Option<String>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let permit = tokio::select! {
            biased;
            () = cancelled.cancelled() => return (None, None),
            () = tokio::time::sleep_until(deadline.into()) => {
                return (None, self.notice(&super::shell::LoginCaptureError::Timeout.to_string()));
            }
            permit = Arc::clone(&self.capture).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return (None, None),
            },
        };
        let generation = self.generation.load(Ordering::Acquire);
        let cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        #[cfg(test)]
        let supplied = self.source.clone();
        #[cfg(not(test))]
        let supplied: Option<Source> = None;
        #[cfg(all(test, unix))]
        let inspection = self.inspection.lock().unwrap().take();
        let checked = tokio::task::spawn_blocking(move || {
            #[cfg(all(test, unix))]
            if let Some(inspection) = inspection {
                let _ = inspection.entered.send(());
                let _ = inspection.release.recv();
            }
            let source = supplied.or_else(Source::current);
            let files = source
                .as_ref()
                .map(|source| source.stamps())
                .unwrap_or_default();
            let valid = cached.filter(|cached| {
                cached.generation == generation
                    && Some(&cached.source) == source.as_ref()
                    && cached.files == files
            });
            (source, files, valid, permit)
        });
        let checked = tokio::select! {
            biased;
            () = cancelled.cancelled() => return (None, None),
            () = tokio::time::sleep_until(deadline.into()) => {
                return (None, self.notice(&super::shell::LoginCaptureError::Timeout.to_string()));
            }
            checked = checked => checked,
        };
        let Ok((source, files, cached, permit)) = checked else {
            return (None, self.notice("startup-file inspection failed"));
        };
        if let Some(cached) = cached {
            return (cached.environment, None);
        }
        let Some(source) = source else {
            return (None, None);
        };
        let result = super::shell::capture_login_environment(
            workspace, &source, producer, cancelled, process, deadline,
        )
        .await;
        if cancelled.is_cancelled() {
            return (None, None);
        }
        let (environment, notice) = match result {
            Ok(environment) => (Some(environment), None),
            Err(error) => (None, self.notice(&error.to_string())),
        };
        *self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Cached {
            generation,
            source,
            files,
            environment: environment.clone(),
        });
        drop(permit);
        (environment, notice)
    }

    fn notice(&self, reason: &str) -> Option<String> {
        (!self.warned.swap(true, Ordering::AcqRel)).then(|| format!(
            "notice: login shell environment skipped ({reason}); commands use the inherited environment. Use /shell reload to retry or shell.login_environment: false to disable."
        ))
    }
}

fn fish_config_path(home: &std::path::Path, config: Option<OsString>) -> PathBuf {
    config
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("fish/config.fish")
}

impl Source {
    fn current() -> Option<Self> {
        #[cfg(unix)]
        {
            let shell = std::env::var_os("SHELL").filter(|shell| !shell.is_empty())?;
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let name = PathBuf::from(&shell)
                .file_name()?
                .to_string_lossy()
                .into_owned();
            let mut startup = vec![PathBuf::from("/etc/profile")];
            if let Some(home) = &home {
                startup.extend(
                    [
                        ".profile",
                        ".bash_profile",
                        ".bash_login",
                        ".bashrc",
                        ".zshenv",
                        ".zprofile",
                        ".zshrc",
                        ".zlogin",
                    ]
                    .map(|file| home.join(file)),
                );
                startup.push(fish_config_path(home, std::env::var_os("XDG_CONFIG_HOME")));
            }
            match name.as_str() {
                "bash" => startup.extend(["/etc/bash.bashrc", "/etc/bashrc"].map(PathBuf::from)),
                "zsh" => {
                    for base in ["/etc", "/etc/zsh"] {
                        startup.extend(
                            ["zshenv", "zprofile", "zshrc", "zlogin"]
                                .map(|file| PathBuf::from(base).join(file)),
                        );
                    }
                    if let Some(dir) = std::env::var_os("ZDOTDIR") {
                        startup.extend(
                            [".zshenv", ".zprofile", ".zshrc", ".zlogin"]
                                .map(|file| PathBuf::from(&dir).join(file)),
                        );
                    }
                }
                "fish" => startup.push(PathBuf::from("/etc/fish/config.fish")),
                _ => {}
            }
            Some(Self {
                shell,
                home,
                startup,
            })
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    fn stamps(&self) -> Vec<(PathBuf, Option<SystemTime>)> {
        self.startup
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    std::fs::metadata(path)
                        .and_then(|metadata| metadata.modified())
                        .ok(),
                )
            })
            .collect()
    }
}
