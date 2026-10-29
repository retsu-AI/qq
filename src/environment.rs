//! Where QQ runs and what its commands can reach, in the words `qq doctor`,
//! the TUI's first screen, and the headless goal note share.
//!
//! Commands run as the user, with the user's network and credentials. The
//! only isolation is whatever QQ itself runs inside, so these lines say
//! which: the host, or a container.

use std::path::Path;

/// The guide page on isolating unattended runs.
pub const UNATTENDED_GUIDE: &str = "https://retsu-ai.github.io/qq/docs/running-unattended/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Host,
    /// Inside a container; the marker that says so.
    Container(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub placement: Placement,
    /// The account commands run as.
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

    /// [`Self::detect`] on a blocking thread: it stats container markers.
    pub async fn detect_off_workers() -> Self {
        tokio::task::spawn_blocking(Self::detect)
            .await
            .unwrap_or_else(|_| Self {
                placement: Placement::Host,
                user: "your user".to_owned(),
            })
    }

    /// `on the host: commands run as …`, for the doctor and the TUI.
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

    /// The one-line stderr note before a headless goal on the host; `None`
    /// inside a container.
    pub fn goal_note(&self) -> Option<String> {
        (self.placement == Placement::Host).then(|| {
            format!(
                "note: this goal runs on the host as {}, with this machine's network and \
                 credentials; to isolate unattended goals see {UNATTENDED_GUIDE}",
                self.user
            )
        })
    }
}

/// Docker writes `/.dockerenv`, Podman `/run/.containerenv`; systemd-nspawn,
/// Podman and Flatpak set `container`; Kubernetes sets its service host.
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

/// `on: …` / `off: …`, after "snapshots" in the doctor and the TUI.
pub fn snapshot_summary(covered: bool) -> &'static str {
    if covered {
        "on: `qq undo` restores the workspace as it was before each run"
    } else {
        "off: not a Git work tree, so `qq undo` cannot restore changes here"
    }
}

/// The lines the TUI's first screen shows under its hints.
pub fn first_run_lines(environment: &Environment, snapshots: bool) -> Vec<String> {
    vec![
        format!("QQ is {}.", environment.summary()),
        format!("Snapshots {}.", snapshot_summary(snapshots)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn only_a_host_goal_gets_the_unattended_note() {
        let note = host("ada").goal_note().unwrap();
        assert_eq!(
            note,
            "note: this goal runs on the host as ada, with this machine's network and \
             credentials; to isolate unattended goals see \
             https://retsu-ai.github.io/qq/docs/running-unattended/"
        );
        assert!(!note.contains('\n'));
        let container = Environment {
            placement: Placement::Container("/.dockerenv"),
            user: "agent".to_owned(),
        };
        assert_eq!(container.goal_note(), None);
    }

    #[test]
    fn the_guide_link_names_a_guide_page() {
        let page = UNATTENDED_GUIDE
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap();
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/guide")
                .join(format!("{page}.md"))
                .is_file(),
            "{UNATTENDED_GUIDE} has no docs/guide/{page}.md"
        );
    }

    #[test]
    fn first_run_lines_name_placement_and_snapshots() {
        assert_eq!(
            first_run_lines(&host("ada"), false),
            [
                "QQ is on the host: commands run as ada, with this machine's network and credentials.",
                "Snapshots off: not a Git work tree, so `qq undo` cannot restore changes here.",
            ]
        );
    }
}
