use std::{process::Stdio, time::Duration};

use base64::Engine as _;
use tokio::io::AsyncWriteExt;

pub(crate) const HELPER_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Helper {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Environment {
    pub tmux: bool,
    pub ssh: bool,
    pub wayland: bool,
    pub x11: bool,
    pub wsl: bool,
    pub macos: bool,
}

impl Environment {
    pub(crate) fn detect() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        Self {
            tmux: set("TMUX"),
            ssh: set("SSH_CONNECTION") || set("SSH_TTY"),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            wsl: set("WSL_DISTRO_NAME"),
            macos: cfg!(target_os = "macos"),
        }
    }

    pub(crate) fn helpers(&self) -> Vec<Helper> {
        let mut helpers = Vec::new();
        if !self.ssh {
            if self.macos {
                helpers.push(Helper {
                    program: "pbcopy",
                    args: &[],
                });
            }
            if self.wayland {
                helpers.push(Helper {
                    program: "wl-copy",
                    args: &[],
                });
            }
            if self.x11 {
                helpers.push(Helper {
                    program: "xclip",
                    args: &["-selection", "clipboard"],
                });
                helpers.push(Helper {
                    program: "xsel",
                    args: &["--clipboard", "--input"],
                });
            }
            if self.wsl {
                helpers.push(Helper {
                    program: "clip.exe",
                    args: &[],
                });
            }
        }
        if self.tmux {
            helpers.push(Helper {
                program: "tmux",
                args: &["load-buffer", "-w", "-"],
            });
        }
        helpers
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Clipboard {
    pub helpers: Vec<Helper>,
    pub tmux: bool,
}

impl Clipboard {
    pub(crate) fn detect() -> Self {
        let environment = Environment::detect();
        Self {
            helpers: environment.helpers(),
            tmux: environment.tmux,
        }
    }
}

pub(crate) fn osc52(text: &str, tmux: bool) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    if tmux {
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\").into_bytes()
    } else {
        format!("\x1b]52;c;{encoded}\x07").into_bytes()
    }
}

pub(crate) async fn run_helpers(helpers: Vec<Helper>, text: String) -> bool {
    for helper in helpers {
        let attempt = tokio::time::timeout(HELPER_TIMEOUT, write_to(&helper, &text)).await;
        if matches!(attempt, Ok(true)) {
            return true;
        }
    }
    false
}

async fn write_to(helper: &Helper, text: &str) -> bool {
    let Ok(mut child) = tokio::process::Command::new(helper.program)
        .args(helper.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        return false;
    };
    if stdin.write_all(text.as_bytes()).await.is_err() {
        return false;
    }
    drop(stdin);
    child.wait().await.is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encodes_the_text_and_wraps_it_for_tmux() {
        assert_eq!(osc52("hi\n", false), b"\x1b]52;c;aGkK\x07".to_vec());
        assert_eq!(
            osc52("hi\n", true),
            b"\x1bPtmux;\x1b\x1b]52;c;aGkK\x07\x1b\\".to_vec()
        );
    }

    #[test]
    fn helpers_follow_the_session_and_skip_local_tools_over_ssh() {
        let local = Environment {
            wayland: true,
            x11: true,
            tmux: true,
            ..Environment::default()
        };
        let programs: Vec<&str> = local
            .helpers()
            .iter()
            .map(|helper| helper.program)
            .collect();
        assert_eq!(programs, ["wl-copy", "xclip", "xsel", "tmux"]);
        let remote = Environment {
            ssh: true,
            x11: true,
            tmux: true,
            ..Environment::default()
        };
        let programs: Vec<&str> = remote
            .helpers()
            .iter()
            .map(|helper| helper.program)
            .collect();
        assert_eq!(programs, ["tmux"]);
        let wsl = Environment {
            wsl: true,
            ..Environment::default()
        };
        assert_eq!(wsl.helpers()[0].program, "clip.exe");
        assert!(Environment::default().helpers().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_first_helper_that_accepts_the_text_confirms_the_copy() {
        let helpers = vec![
            Helper {
                program: "false",
                args: &[],
            },
            Helper {
                program: "cat",
                args: &[],
            },
        ];
        assert!(run_helpers(helpers, "text".to_owned()).await);
    }

    #[tokio::test]
    async fn a_missing_helper_is_not_a_confirmed_copy() {
        let helpers = vec![Helper {
            program: "qq-no-such-clipboard-helper",
            args: &[],
        }];
        assert!(!run_helpers(helpers, "text".to_owned()).await);
    }
}
