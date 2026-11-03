use std::path::Path;

use qq_core::undo::{Coverage, MINIMUM_GIT_VERSION};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Host,
    Container(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub placement: Placement,
    pub user: String,
}

impl Environment {
    pub fn detect() -> Self {
        let env = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        Self {
            placement: placement(|path| Path::new(path).exists(), env),
            user: env("USER").or_else(|| env("USERNAME")).map_or_else(
                || "your user".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
        }
    }

    pub async fn detect_off_workers() -> Self {
        tokio::task::spawn_blocking(Self::detect)
            .await
            .unwrap_or_else(|_| Self {
                placement: Placement::Host,
                user: "your user".to_owned(),
            })
    }

    pub fn summary(&self) -> String {
        match self.placement {
            Placement::Host => format!(
                "on the host: commands run as {}, with this machine's network and credentials",
                self.user
            ),
            Placement::Container(marker) => format!(
                "in a container ({marker}): commands run as {}, with the container's network \
                 and the credentials passed into it",
                self.user
            ),
        }
    }
}

fn placement(
    exists: impl Fn(&str) -> bool,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Placement {
    for marker in ["/.dockerenv", "/run/.containerenv"] {
        if exists(marker) {
            return Placement::Container(marker);
        }
    }
    if env("container").is_some() {
        return Placement::Container("$container");
    }
    if env("KUBERNETES_SERVICE_HOST").is_some() {
        return Placement::Container("Kubernetes");
    }
    Placement::Host
}

pub fn snapshot_summary(coverage: Coverage) -> String {
    match coverage {
        Coverage::Covered => {
            "on: `qq undo` restores the workspace as it was before each run".to_owned()
        }
        Coverage::NotGit => {
            "off: not a Git work tree, so `qq undo` cannot restore changes here".to_owned()
        }
        Coverage::GitTooOld(found) => format!(
            "off: Git {found} is older than {MINIMUM_GIT_VERSION}, so `qq undo` cannot restore changes"
        ),
    }
}

pub fn first_run_lines(environment: &Environment, snapshots: Coverage) -> Vec<String> {
    vec![
        format!("QQ is {}.", environment.summary()),
        format!("Snapshots {}.", snapshot_summary(snapshots)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use qq_core::undo::GitVersion;

    fn host(user: &str) -> Environment {
        Environment {
            placement: Placement::Host,
            user: user.to_owned(),
        }
    }

    #[test]
    fn container_markers_win_over_the_host() {
        let none = |_: &str| None;
        assert_eq!(placement(|_| false, none), Placement::Host);
        assert_eq!(
            placement(|path| path == "/.dockerenv", none),
            Placement::Container("/.dockerenv")
        );
        assert_eq!(
            placement(|path| path == "/run/.containerenv", none),
            Placement::Container("/run/.containerenv")
        );
        assert_eq!(
            placement(
                |_| false,
                |name| (name == "container").then(|| "podman".into())
            ),
            Placement::Container("$container")
        );
        assert_eq!(
            placement(
                |_| false,
                |name| (name == "KUBERNETES_SERVICE_HOST").then(|| "10.0.0.1".into())
            ),
            Placement::Container("Kubernetes")
        );
    }

    #[test]
    fn the_host_says_whose_network_and_credentials_commands_use() {
        assert_eq!(
            host("ada").summary(),
            "on the host: commands run as ada, with this machine's network and credentials"
        );
        assert_eq!(
            Environment {
                placement: Placement::Container("/.dockerenv"),
                user: "agent".to_owned(),
            }
            .summary(),
            "in a container (/.dockerenv): commands run as agent, with the container's \
             network and the credentials passed into it"
        );
    }

    #[test]
    fn first_run_lines_name_placement_and_snapshots() {
        assert_eq!(
            first_run_lines(&host("ada"), Coverage::NotGit),
            [
                "QQ is on the host: commands run as ada, with this machine's network and credentials.",
                "Snapshots off: not a Git work tree, so `qq undo` cannot restore changes here.",
            ]
        );
        assert_eq!(
            first_run_lines(
                &host("ada"),
                Coverage::GitTooOld(GitVersion {
                    major: 2,
                    minor: 31
                })
            )[1],
            "Snapshots off: Git 2.31 is older than 2.32, so `qq undo` cannot restore changes."
        );
    }
}
