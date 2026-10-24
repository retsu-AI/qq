use super::*;
use qq_core::command_owner::{NativeDirectoryGrant, NativeEnvironment};
use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

fn command(mut command: Command, input: &Path) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    command
        .stdin(Stdio::from(fs::File::open(input)?))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let result = (|| {
        let stdout = capture_bounded(
            child.stdout.take().ok_or_else(|| err("stdout missing"))?,
            "alias-stdout",
        )?;
        let stderr = capture_bounded(
            child.stderr.take().ok_or_else(|| err("stderr missing"))?,
            "alias-stderr",
        )?;
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                return Err(err("alias command exceeded bounded wait"));
            }
            std::thread::yield_now();
        };
        let stdout = stdout.recv_timeout(Duration::from_secs(5)).map_err(err)??;
        let stderr = stderr.recv_timeout(Duration::from_secs(5)).map_err(err)??;
        Ok((status.code().unwrap_or(-1), stdout, stderr))
    })();
    if let Err(error) = result {
        let killed = child.kill();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    return Err(err(format!(
                        "{error}; command leader reaped; outer job still owns descendants"
                    )));
                }
                Ok(None) if std::time::Instant::now() < deadline => std::thread::yield_now(),
                status => {
                    return Err(err(format!(
                        "{error}; command cleanup unconfirmed: kill={killed:?}, wait={status:?}"
                    )));
                }
            }
        }
    }
    result
}
fn system_image(name: &str) -> io::Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("SystemRoot").ok_or_else(|| err("SystemRoot missing"))?)
            .join("System32")
            .join(name),
    )
}
struct Mapping {
    drive: String,
    target: NativeDirectoryGrant,
    input: PathBuf,
    active: bool,
}
impl Mapping {
    fn create(target: &Path, input: &Path) -> io::Result<Self> {
        let used = winsafe::GetLogicalDrives();
        if used == 0 {
            return Err(err("GetLogicalDrives returned no drives"));
        }
        let letter = (b'D'..=b'Z')
            .rev()
            .find(|letter| used & (1 << (letter - b'A')) == 0)
            .ok_or_else(|| err("no unused local drive"))?;
        let drive = format!("{}:", char::from(letter));
        let target_grant = NativeDirectoryGrant::capture(&target.canonicalize()?)?;
        // Install the ownership guard before the side effect: a lost output or
        // wait response can still leave a successfully created mapping behind.
        let mapping = Self {
            drive,
            target: target_grant,
            input: input.to_owned(),
            active: true,
        };
        let mut create = Command::new(system_image("subst.exe")?);
        create.arg(&mapping.drive).arg(target);
        let (code, out, error) = command(create, input)?;
        if code != 0 {
            return Err(err(format!(
                "subst create exit={code}, stdout={out:?}, stderr={error:?}"
            )));
        }
        mapping.verify()?;
        println!(
            "owned alias created {} -> {}",
            mapping.drive,
            mapping.target.path.display()
        );
        Ok(mapping)
    }
    fn root(&self) -> PathBuf {
        PathBuf::from(format!("{}\\", self.drive))
    }
    fn verify(&self) -> io::Result<()> {
        let list = Command::new(system_image("subst.exe")?);
        let (code, out, error) = command(list, &self.input)?;
        if code != 0 {
            return Err(err(format!("subst query exit={code}, stderr={error:?}")));
        }
        let text = std::str::from_utf8(&out).map_err(err)?;
        let prefix = format!("{}\\: => ", self.drive);
        let target = text
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .ok_or_else(|| err("owned subst mapping missing"))?;
        if Path::new(target).canonicalize()? != self.target.path
            || NativeDirectoryGrant::capture(&self.target.path)?.identity != self.target.identity
            || NativeDirectoryGrant::capture(&self.root())?.identity != self.target.identity
        {
            return Err(err("owned subst mapping target or identity changed"));
        }
        Ok(())
    }
    fn remove(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // CLI subst has no atomic EXACT_MATCH_ON_REMOVE option. This diagnostic
        // rechecks the unique target, never removes an unknown mapping, and makes
        // no production ownership guarantee against other same-logon writers.
        self.verify()?;
        let mut remove = Command::new(system_image("subst.exe")?);
        remove.arg(&self.drive).arg("/D");
        let (code, out, error) = command(remove, &self.input)?;
        if code != 0 {
            return Err(err(format!(
                "subst remove exit={code}, stdout={out:?}, stderr={error:?}"
            )));
        }
        self.active = false;
        println!("owned alias removed {}", self.drive);
        Ok(())
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.remove()
        {
            eprintln!("owned diagnostic alias cleanup refused/failed: {error}");
        }
    }
}
fn stage_selected(
    source: &Path,
    root: &Path,
    name: &str,
    selected: &[PathBuf],
    max_bytes: u64,
) -> io::Result<PathBuf> {
    use std::os::windows::fs::MetadataExt as _;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut pending = selected.to_vec();
    let mut count = 0usize;
    let mut bytes = 0u64;
    while let Some(relative) = pending.pop() {
        count += 1;
        if count > 65_536
            || std::time::Instant::now() >= deadline
            || relative.components().count() > 64
        {
            return Err(err(format!(
                "{name} metadata traversal limit: entries={count}, bytes={bytes}"
            )));
        }
        let path = source.join(&relative);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_attributes() & 0x400 != 0 || (!metadata.is_file() && !metadata.is_dir()) {
            return Err(err(format!(
                "{name} unsupported alias/object: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(relative.join(entry?.file_name()));
                if pending.len() + count > 65_536 {
                    return Err(err(format!("{name} metadata entry limit")));
                }
            }
        } else {
            use cap_primitives::fs::_WindowsByHandle as _;
            let held = cap_std::fs::File::from_std(fs::File::open(&path)?);
            if held.metadata()?.number_of_links() != Some(1) {
                return Err(err(format!("{name} hardlinked file: {}", path.display())));
            }
            bytes = bytes
                .checked_add(metadata.len())
                .ok_or_else(|| err("SDK size overflow"))?;
        }
    }
    println!(
        "{name} selected source={} selections={selected:?} entries={count} bytes={bytes} byte_cap={max_bytes}",
        source.display()
    );
    qq_core::task_workspace::stage_tool_inputs(
        NativeDirectoryGrant::capture(source)?, &root.join(name), selected,
        qq_core::task_workspace::TaskWorkspaceLimits { max_entries: 65_536, max_bytes },
        std::time::Instant::now() + Duration::from_secs(60),
    ).map(|grant| grant.path).map_err(|error| err(format!("{name} staging failed: source={} entries={count} bytes={bytes} byte_cap={max_bytes}: {error}", source.display())))
}
fn stage_node(root: &Path) -> io::Result<PathBuf> {
    let node =
        PathBuf::from(std::env::var_os("ProgramFiles").ok_or_else(|| err("ProgramFiles missing"))?)
            .join("nodejs")
            .canonicalize()?;
    stage_selected(
        &node,
        root,
        "node-stage",
        &[
            "node.exe".into(),
            "npm.cmd".into(),
            "npx.cmd".into(),
            "node_modules/npm".into(),
        ],
        256 * 1024 * 1024,
    )
}
fn stage_rust(root: &Path) -> io::Result<PathBuf> {
    let bin = NativeEnvironment::windows_toolchain_bins()
        .into_iter()
        .find(|p| p.join("cargo.exe").is_file() && p.join("rustc.exe").is_file())
        .ok_or_else(|| err("installed direct Rust toolchain unavailable"))?;
    let source = bin
        .parent()
        .ok_or_else(|| err("Rust toolchain parent missing"))?;
    stage_selected(
        source,
        root,
        "rust-stage",
        &["bin".into(), "lib".into()],
        1024 * 1024 * 1024,
    )
    .map(|root| root.join("bin"))
}
fn grant_sdk(root: &Path, user: &str, package: &str) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;
    let mut pending = vec![root.to_owned()];
    let mut count = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while let Some(path) = pending.pop() {
        count += 1;
        if count > 65_536 || std::time::Instant::now() >= deadline {
            return Err(err("owned SDK ACL preparation limit"));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_attributes() & 0x400 != 0 || (!metadata.is_file() && !metadata.is_dir()) {
            return Err(err("owned SDK contains a reparse or unsupported object"));
        }
        let inheritance = if metadata.is_dir() { "OICI" } else { "" };
        set_acl(
            &path,
            &format!("D:P(A;{inheritance};FA;;;{user})(A;{inheritance};FRFX;;;{package})"),
        )?;
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}
fn write_projects(task: &Path) -> io::Result<()> {
    fs::write(
        task.join("package.json"),
        r#"{"private":true,"scripts":{"build":"node build.js","test":"node test.js"}}"#,
    )?;
    fs::write(
        task.join("build.js"),
        "require('fs').writeFileSync('built.json',JSON.stringify({answer:42}));",
    )?;
    fs::write(
        task.join("test.js"),
        "require('assert').strictEqual(JSON.parse(require('fs').readFileSync('built.json')).answer,42);",
    )?;
    fs::create_dir_all(task.join("rust-proof/src"))?;
    fs::write(
        task.join("rust-proof/Cargo.toml"),
        "[package]\nname='alias-proof'\nversion='0.1.0'\nedition='2024'\n",
    )?;
    fs::write(
        task.join("rust-proof/src/main.rs"),
        "fn main(){println!(\"alias-proof\");} #[test]fn answer(){assert_eq!(6*7,42);}",
    )?;
    fs::write(
        task.join("realpath.js"),
        r#"const fs=require('fs');let out={execPath:process.execPath,cwd:process.cwd()};for(const [name,fn]of Object.entries({root:()=>fs.lstatSync(require('path').parse(process.cwd()).root).isDirectory(),physical_root:()=>fs.lstatSync('C:\\').isDirectory(),js:()=>fs.realpathSync('realpath.js'),native:()=>fs.realpathSync.native('realpath.js')})){try{out[name]={ok:true,value:fn()}}catch(e){out[name]={ok:false,code:e.code,path:e.path}}}console.log(JSON.stringify(out));"#,
    )?;
    fs::write(task.join("input-empty.txt"), b"")?;
    Ok(())
}
fn run_tools(root: &Path, label: &str, node: &Path, rust: Option<&Path>) -> io::Result<Vec<i32>> {
    use std::os::windows::process::CommandExt as _;
    let task = root.join("task");
    let home = task.join("home");
    let system = system_image("cmd.exe")?;
    let path = std::env::join_paths([Some(node), rust, system.parent()].into_iter().flatten())
        .map_err(err)?;
    let mut statuses = Vec::new();
    for (name, text) in [
        ("node", "node realpath.js"),
        ("npm-build", "npm.cmd run build"),
        ("npm-test", "npm.cmd test"),
        (
            "cargo-build",
            "cargo build --offline --manifest-path rust-proof/Cargo.toml",
        ),
        (
            "cargo-test",
            "cargo test --offline --manifest-path rust-proof/Cargo.toml",
        ),
    ] {
        if name.starts_with("cargo-") && rust.is_none() {
            continue;
        }
        let mut cmd = Command::new(&system);
        cmd.raw_arg(format!("/D /S /C \"{text}\""));
        cmd.current_dir(&task)
            .env_clear()
            .env(
                "SystemRoot",
                std::env::var_os("SystemRoot").ok_or_else(|| err("SystemRoot missing"))?,
            )
            .env("COMSPEC", &system)
            .env("PATH", &path)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("APPDATA", home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", home.join("AppData/Local"))
            .env("TEMP", home.join("tmp"))
            .env("TMP", home.join("tmp"))
            .env("CARGO_TARGET_DIR", task.join(format!("target-{label}")))
            .env("CARGO_HOME", home.join(".cargo"))
            .env("RUSTUP_HOME", home.join(".rustup"));
        if let Some(rust) = rust {
            cmd.env("RUSTC", rust.join("rustc.exe"));
        }
        let (code, out, error) = command(cmd, &task.join("input-empty.txt"))?;
        println!(
            "{label} {name}: exit={code:#x}; stdout={}; stderr={}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&error)
        );
        fs::write(task.join(format!("{label}-{name}.stdout")), &out)?;
        fs::write(task.join(format!("{label}-{name}.stderr")), &error)?;
        statuses.push(code);
    }
    Ok(statuses)
}
fn import_names(task: &Path, rust: &Path) -> io::Result<()> {
    let llvm =
        PathBuf::from(std::env::var_os("ProgramFiles").ok_or_else(|| err("ProgramFiles missing"))?)
            .join("LLVM")
            .join("bin")
            .join("llvm-readobj.exe");
    if !llvm.is_file() {
        println!("exact staged PE import telemetry unavailable: LLVM readobj not installed");
        return Ok(());
    }
    let script = task.join("pe-imports.ps1");
    fs::write(
        &script,
        "param([string]$Llvm,[string]$Image)\n$ErrorActionPreference='Stop'\n& $Llvm --coff-imports $Image | Select-String -Pattern '^\\s*Name:' | ForEach-Object { $_.Line.Trim() }\nexit $LASTEXITCODE\n",
    )?;
    let shell = system_image("WindowsPowerShell")?
        .join("v1.0")
        .join("powershell.exe");
    for name in ["cargo.exe", "rustc.exe"] {
        let image = rust.join(name);
        let mut query = Command::new(&shell);
        query
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg(&llvm)
            .arg(&image);
        let (code, out, error) = command(query, &task.join("input-empty.txt"))?;
        println!(
            "exact staged image={} PE import names: exit={code}, stdout={}, stderr={}, bounded_complete={}",
            image.display(),
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&error),
            out.len() < 16 * 1024
        );
    }
    Ok(())
}
pub(super) fn environment(root: &Path) -> io::Result<Vec<(OsString, OsString)>> {
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("task/tool-paths.json"))?)?;
    let get = |key: &str| {
        config[key]
            .as_str()
            .map(OsString::from)
            .ok_or_else(|| err("tool input path missing"))
    };
    let system = system_image("cmd.exe")?;
    let mut paths = vec![
        PathBuf::from(get("node")?),
        system.parent().unwrap().to_owned(),
    ];
    if let Some(rust) = config["rust"].as_str() {
        paths.insert(1, PathBuf::from(rust));
    }
    Ok(vec![
        ("PATH".into(), std::env::join_paths(paths).map_err(err)?),
        ("COMSPEC".into(), system.into_os_string()),
    ])
}
fn node_run(
    parent: &Path,
    user: &str,
    write_sid: &str,
    package: &rappct::AppContainerSid,
) -> io::Result<()> {
    let root = parent.join("alias-root");
    fs::create_dir(&root)?;
    // This is a fresh owned directory, never the physical drive root.
    set_acl(
        &root,
        &format!("D:P(A;OICI;FA;;;{user})(A;;0x001000a0;;;{package})"),
    )?;
    let task = root.join("task");
    fs::create_dir(&task)?;
    set_acl(
        &task,
        &format!("D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;{package})(A;OICI;FA;;;{write_sid})"),
    )?;
    for name in [
        "home/tmp",
        "home/AppData/Local",
        "home/AppData/Roaming",
        "home/.cargo",
        "home/.rustup",
    ] {
        fs::create_dir_all(task.join(name))?;
    }
    let sdk = root.join("sdk");
    fs::create_dir(&sdk)?;
    set_acl(
        &sdk,
        &format!("D:P(A;OICI;FA;;;{user})(A;OICI;FRFX;;;{package})"),
    )?;
    let node = stage_node(&sdk)?;
    fs::write(
        task.join("node-stage-path.json"),
        serde_json::to_vec(&node)?,
    )?;
    grant_sdk(&node, user, package.as_string())?;
    fs::copy(std::env::current_exe()?, task.join("owned-payload.exe"))?;
    fs::write(task.join("expected-write-sid.txt"), write_sid)?;
    fs::copy(
        parent.join("launch-identities.json"),
        root.join("launch-identities.json"),
    )?;
    set_acl(
        &root.join("launch-identities.json"),
        &format!("D:P(A;;FA;;;{user})"),
    )?;
    for (name, bytes, rights) in [
        ("private.txt", b"private-canary".as_slice(), ""),
        (
            "public.txt",
            b"public-input".as_slice(),
            "(A;;FR;;;S-1-15-2-1)",
        ),
        (
            "outside.txt",
            b"outside-original".as_slice(),
            "(A;;FA;;;S-1-15-2-1)",
        ),
    ] {
        fs::write(root.join(name), bytes)?;
        set_acl(&root.join(name), &format!("D:P(A;;FA;;;{user}){rights}"))?;
    }
    write_projects(&task)?;
    let host = run_tools(&root, "host", &node, None)?;
    if host.iter().any(|code| *code != 0) {
        return Err(err(format!(
            "same staged SDK host positive control failed: {host:?}"
        )));
    }
    let mut mapping = Mapping::create(&root, &task.join("input-empty.txt"))?;
    let alias = mapping.root();
    let aliased = |path: &Path| -> io::Result<PathBuf> {
        Ok(alias.join(path.strip_prefix(&root).map_err(err)?))
    };
    fs::write(
        task.join("tool-paths.json"),
        serde_json::to_vec(&serde_json::json!({"node":aliased(&node)?,"rust":null}))?,
    )?;
    let control_ui = PrivateUi::create(
        &format!("qq-alias-control-{}", std::process::id()),
        user,
        user,
        package.as_string(),
    )?;
    let control = launch_lpac_as(&alias, "alias-control", None, &control_ui.path);
    if control.is_err() {
        std::mem::forget(control_ui);
    } else {
        drop(control_ui);
    }
    let outcome = control.and_then(|()| {
        let controlled: serde_json::Value =
            serde_json::from_slice(&fs::read(task.join("alias-control-result.json"))?)?;
        if controlled["outside_write_allowed"] != true
            || controlled["outside_dac_allowed"] != true
            || controlled["outside_owner_allowed"] != true
            || controlled["private_denied"] != true
        {
            return Err(err(
                "ordinary AppContainer same-canary controls were not exercised",
            ));
        }
        if fs::read(root.join("outside.txt"))? != b"unexpected" {
            return Err(err(
                "ordinary AppContainer alias control did not exercise outside write",
            ));
        }
        fs::write(root.join("outside.txt"), b"outside-original")?;
        let ui = PrivateUi::create(
            &format!("qq-alias-{}", std::process::id()),
            user,
            write_sid,
            package.as_string(),
        )?;
        let launched = launch_restricted_payload(&alias, write_sid, &ui.path, "alias");
        if launched.is_err() {
            std::mem::forget(ui);
        } else {
            drop(ui);
        }
        launched
    });
    let cleanup = mapping.remove();
    outcome?;
    cleanup?;
    let result: serde_json::Value =
        serde_json::from_slice(&fs::read(task.join("alias-result.json"))?)?;
    println!("alias actual result: {result}");
    let paths: serde_json::Value =
        serde_json::from_slice(&fs::read(task.join("alias-node.stdout"))?)?;
    println!("alias actual path spellings: {paths}");
    if paths["root"]["ok"] != true || paths["js"]["ok"] != true {
        return Err(err(
            "alias root metadata or JS realpath remained unavailable",
        ));
    }

    if result["task_write"] != true
        || result["private_denied"] != true
        || result["outside_write_denied"] != true
        || result["outside_dac_denied"] != true
        || result["outside_owner_denied"] != true
        || result["root_write_denied"] != true
        || fs::read(root.join("outside.txt"))? != b"outside-original"
        || result["tool_codes"] != serde_json::json!([0, 0, 0])
    {
        return Err(err("alias workflow or native negative control failed"));
    }
    Ok(())
}
fn cargo_run(
    parent: &Path,
    user: &str,
    write_sid: &str,
    package: &rappct::AppContainerSid,
    rust: &Path,
) -> io::Result<()> {
    let root = parent.join("alias-root");
    let task = root.join("task");
    let node: PathBuf = serde_json::from_slice(&fs::read(task.join("node-stage-path.json"))?)?;
    grant_sdk(
        rust.parent().ok_or_else(|| err("Rust root missing"))?,
        user,
        package.as_string(),
    )?;
    import_names(&task, rust)?;
    let host = run_tools(&root, "cargo-host", &node, Some(rust))?;
    if host.iter().any(|code| *code != 0) {
        return Err(err(format!(
            "same staged Cargo host control failed: {host:?}"
        )));
    }
    let mut mapping = Mapping::create(&root, &task.join("input-empty.txt"))?;
    let alias = mapping.root();
    let aliased = |path: &Path| -> io::Result<PathBuf> {
        Ok(alias.join(path.strip_prefix(&root).map_err(err)?))
    };
    fs::write(
        task.join("tool-paths.json"),
        serde_json::to_vec(&serde_json::json!({"node":aliased(&node)?,"rust":aliased(rust)?}))?,
    )?;
    fs::write(root.join("outside.txt"), b"outside-original")?;
    let control_ui = std::mem::ManuallyDrop::new(PrivateUi::create(
        &format!("qq-cargo-control-{}", std::process::id()),
        user,
        user,
        package.as_string(),
    )?);
    let outcome = (|| {
        launch_lpac_as(&alias, "alias-control", None, &control_ui.path)?;
        let control: serde_json::Value =
            serde_json::from_slice(&fs::read(task.join("alias-control-result.json"))?)?;
        if control["outside_write_allowed"] != true
            || control["outside_dac_allowed"] != true
            || control["outside_owner_allowed"] != true
            || control["private_denied"] != true
            || fs::read(root.join("outside.txt"))? != b"unexpected"
        {
            return Err(err("Cargo ordinary control failed"));
        }
        fs::write(root.join("outside.txt"), b"outside-original")?;
        let ui = std::mem::ManuallyDrop::new(PrivateUi::create(
            &format!("qq-cargo-{}", std::process::id()),
            user,
            write_sid,
            package.as_string(),
        )?);
        launch_restricted_payload(&alias, write_sid, &ui.path, "alias")?;
        let result: serde_json::Value =
            serde_json::from_slice(&fs::read(task.join("alias-result.json"))?)?;
        println!("independent Cargo alias result: {result}");
        if [
            "task_write",
            "private_denied",
            "outside_write_denied",
            "outside_dac_denied",
            "outside_owner_denied",
            "root_write_denied",
        ]
        .iter()
        .any(|key| result[*key] != true)
            || result["tool_codes"] != serde_json::json!([0, 0, 0, 0, 0])
            || fs::read(root.join("outside.txt"))? != b"outside-original"
        {
            return Err(err("Cargo alias workflow/negative control failed"));
        }
        Ok(())
    })();
    let cleanup = mapping.remove();
    outcome?;
    cleanup
}
pub(super) fn run(
    parent: &Path,
    user: &str,
    write_sid: &str,
    package: &rappct::AppContainerSid,
) -> io::Result<()> {
    let node = node_run(parent, user, write_sid, package);
    println!("independent Node alias result: {node:?}");
    let sdk = parent.join("alias-root").join("sdk");
    let rust = stage_rust(&sdk).and_then(|bin| cargo_run(parent, user, write_sid, package, &bin));
    println!("independent Cargo closure/workflow result: {rust:?}");
    node?;
    rust
}
pub(super) fn payload(root: &Path, mode: &str, package: &str) -> io::Result<()> {
    let token =
        firehazard::open_process_token(firehazard::get_current_process(), firehazard::token::QUERY)
            .map_err(err)?;
    let restricted = token.restricted_sids().map_err(err)?;
    let expected = firehazard::convert_string_sid_to_sid_a(
        fs::read_to_string(root.join("task/expected-write-sid.txt"))?.as_str(),
    )
    .map_err(err)?;
    let app = rappct::token::query_current_process_token().map_err(err)?;
    if !app.is_appcontainer
        || app
            .package_sid
            .as_ref()
            .is_none_or(|sid| sid.as_string() != package)
        || restricted.group_count() != u32::from(mode == "alias")
        || restricted
            .groups()
            .iter()
            .any(|g| firehazard::equal_sid(g.sid, (&expected).into()))
            != (mode == "alias")
    {
        return Err(err("alias payload token identity mismatch"));
    }
    println!("{mode} actual AppContainer; root={}", root.display());
    let denied = |result: io::Result<fs::File>| {
        result.is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
    };
    let outside = root.join("outside.txt");
    let private_denied = fs::read(root.join("private.txt"))
        .is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied);
    if fs::read(root.join("public.txt"))? != b"public-input" {
        return Err(err("alias OS-public read failed"));
    }
    let outside_write = fs::write(&outside, b"unexpected");
    let outside_write_allowed = outside_write.is_ok();
    let outside_write_denied =
        outside_write.is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied);
    let outside_dac = fs::File::options()
        .access_mode(0x0004_0000)
        .share_mode(7)
        .open(&outside);
    let outside_dac_allowed = outside_dac.is_ok();
    let outside_dac_denied = denied(outside_dac);
    let outside_owner = fs::File::options()
        .access_mode(0x0008_0000)
        .share_mode(7)
        .open(&outside);
    let outside_owner_allowed = outside_owner.is_ok();
    let outside_owner_denied = denied(outside_owner);
    let root_write_denied = denied(
        fs::File::options()
            .access_mode(0x0000_0002)
            .share_mode(7)
            .custom_flags(0x0200_0000)
            .open(root),
    );
    fs::write(root.join("task/alias-task-write"), b"task-write")?;
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("task/tool-paths.json"))?)?;
    let node = PathBuf::from(
        config["node"]
            .as_str()
            .ok_or_else(|| err("node path missing"))?,
    );
    let rust = config["rust"].as_str().map(PathBuf::from);
    let codes = run_tools(root, mode, &node, rust.as_deref())?;
    fs::write(
        root.join(format!("task/{mode}-result.json")),
        serde_json::to_vec(
            &serde_json::json!({"task_write":fs::read(root.join("task/alias-task-write"))?==b"task-write","private_denied":private_denied,"outside_write_allowed":outside_write_allowed,"outside_write_denied":outside_write_denied,"outside_dac_allowed":outside_dac_allowed,"outside_dac_denied":outside_dac_denied,"outside_owner_allowed":outside_owner_allowed,"outside_owner_denied":outside_owner_denied,"root_write_denied":root_write_denied,"tool_codes":codes}),
        )?,
    )?;
    Ok(())
}
