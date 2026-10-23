//! Bounded native feasibility proof; never part of the production launcher.
#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    eprintln!("this proof requires Windows");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    if let Err(error) = proof::run() {
        eprintln!("host-token proof failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod proof {
    use std::{fs, io, os::windows::fs::OpenOptionsExt as _, path::Path, time::Duration};
    use windows_permissions::{
        LocalBox, SecurityDescriptor,
        constants::{SeObjectType, SecurityInformation},
        wrappers::SetSecurityInfo,
    };

    fn err(error: impl std::fmt::Debug) -> io::Error {
        io::Error::other(format!("{error:?}"))
    }

    pub(super) fn run() -> io::Result<()> {
        let arguments: Vec<_> = std::env::args().collect();
        if arguments.get(1).is_some_and(|value| value == "--launcher") {
            let root = Path::new(&arguments[2]);
            let result = launch_lpac(root, &arguments[3], "filtered");
            if let Err(error) = &result {
                fs::write(root.join("task/launcher-error.txt"), error.to_string())?;
                // No console or inherited host stdio is required by this helper.
                std::process::exit(1);
            }
            return result;
        }
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let workspace = root.join("task");
        fs::create_dir(&workspace)?;
        let outside = root.join("outside.txt");
        let public = root.join("public.txt");
        fs::write(&outside, b"original")?;
        fs::write(&public, b"public-input")?;
        let name = format!("qq.host-token-proof.{}", std::process::id());
        let profile =
            rappct::AppContainerProfile::ensure(&name, &name, Some("QQ owned host token proof"))
                .map_err(err)?;
        let outcome = (|| {
            let user = win_custody::current_user_sid_string()?;
            // All ACL changes are confined to these freshly created objects.
            set_acl(
                root,
                &format!("D:P(A;OICI;FA;;;{user})(A;OICI;FRFX;;;S-1-15-2-2)"),
            )?;
            set_acl(
                &workspace,
                &format!("D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;{})", profile.sid),
            )?;
            set_acl(
                &outside,
                &format!("D:P(A;;FA;;;{user})(A;;FRFW;;;S-1-15-2-2)"),
            )?;
            set_acl(&public, &format!("D:P(A;;FA;;;{user})(A;;FR;;;S-1-15-2-2)"))?;
            // Create profile/scratch inputs after installing the task's
            // inheritable package ACL, matching the production task home.
            for relative in ["home/tmp", "home/AppData/Local", "home/AppData/Roaming"] {
                fs::create_dir_all(workspace.join(relative))?;
            }
            for mode in ["control", "filtered"] {
                let script = format!(
                    r#"@echo off
 echo begin
 >stage-{mode}.txt echo begin
 echo task-written>task-{mode}.txt
 set code=%errorlevel%
 >>stage-{mode}.txt echo task:%code%
 if not "%code%"=="0" exit /b 21
 type nul >nul
 set code=%errorlevel%
 >>stage-{mode}.txt echo nul:%code%
 if not "%code%"=="0" exit /b 22
 copy /b "%SystemRoot%\System32\cmd.exe" os-{mode}.bin >nul
 set code=%errorlevel%
 >>stage-{mode}.txt echo os-read:%code%
 if not "%code%"=="0" exit /b 23
 type "{}" >public-{mode}.txt
 set code=%errorlevel%
 >>stage-{mode}.txt echo public-read:%code%
 if not "%code%"=="0" exit /b 24
 (echo outside-written)>"{}"
 set code=%errorlevel%
 >>stage-{mode}.txt echo outside-write:%code%
 if not "%code%"=="0" (echo denied>outside-{mode}.txt) else (echo allowed>outside-{mode}.txt)
 >>stage-{mode}.txt echo completed
 exit /b 0
"#,
                    public.display(),
                    outside.display(),
                )
                .replace('\n', "\r\n");
                fs::write(workspace.join(format!("{mode}.cmd")), script)?;
                set_acl(
                    &workspace.join(format!("{mode}.cmd")),
                    &format!("D:P(A;;FA;;;{user})(A;;FR;;;{})", profile.sid),
                )?;
            }
            // Establish that ordinary LPAC really can perform the canary write.
            launch_lpac(root, &name, "control")?;
            assert_results(root, "control", "allowed")?;
            if fs::read_to_string(&outside)?.trim() != "outside-written" {
                return Err(err(
                    "ordinary LPAC control did not change the ARAP-writable canary",
                ));
            }
            println!("control: task write, NUL, OS read, public read, ARAP outside write passed");
            fs::write(&outside, b"original")?;
            launch_restricted_helper(root, &name, profile.sid.as_string())?;
            assert_results(root, "filtered", "denied")?;
            if fs::read(&outside)? != b"original" {
                return Err(err("filtered LPAC changed the outside canary"));
            }
            println!("filtered: task write, NUL, OS/public reads passed; outside write denied");
            Ok(())
        })();
        let cleanup = profile.delete().map_err(err);
        outcome.and(cleanup)
    }

    fn set_acl(path: &Path, sddl: &str) -> io::Result<()> {
        let security: LocalBox<SecurityDescriptor> = sddl.parse()?;
        let mut file = fs::OpenOptions::new()
            .access_mode(0x0004_0000)
            .share_mode(7)
            .custom_flags(0x0200_0000)
            .open(path)?;
        SetSecurityInfo(
            &mut file,
            SeObjectType::SE_KERNEL_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None,
            None,
            security.dacl(),
            None,
        )
    }

    fn launch_lpac(root: &Path, name: &str, mode: &str) -> io::Result<()> {
        let token = firehazard::open_process_token(
            firehazard::get_current_process(),
            firehazard::token::QUERY,
        )
        .map_err(err)?;
        let count = token.restricted_sids().map_err(err)?.group_count();
        if mode == "control" {
            println!("control launcher: restricting SID count={count}");
        } else {
            fs::write(root.join("task/restricting-count.txt"), count.to_string())?;
        }
        if mode == "filtered" && count != 1 {
            return Err(err("trusted launcher lost its sole task restricting SID"));
        }
        let package = rappct::derive_sid_from_name(name).map_err(err)?;
        let capabilities = rappct::SecurityCapabilitiesBuilder::new(&package)
            .with_named(&["registryRead"])
            .lpac(true)
            .build()
            .map_err(err)?;
        let windows = std::env::var_os("SystemRoot").ok_or_else(|| err("missing SystemRoot"))?;
        let home = root.join("task/home");
        let temporary = home.join("tmp");
        let options = rappct::LaunchOptions {
            exe: Path::new(&windows).join("System32/cmd.exe"),
            cmdline: Some(format!("cmd.exe /d /c {mode}.cmd")),
            cwd: Some(root.join("task")),
            env: Some(rappct::launch::merge_parent_env(vec![
                ("SystemRoot".into(), windows.clone()),
                (
                    "PATH".into(),
                    Path::new(&windows).join("System32").into_os_string(),
                ),
                ("HOME".into(), home.clone().into_os_string()),
                ("USERPROFILE".into(), home.clone().into_os_string()),
                (
                    "LOCALAPPDATA".into(),
                    home.join("AppData/Local").into_os_string(),
                ),
                (
                    "APPDATA".into(),
                    home.join("AppData/Roaming").into_os_string(),
                ),
                ("TMPDIR".into(), temporary.clone().into_os_string()),
                ("TEMP".into(), temporary.clone().into_os_string()),
                ("TMP".into(), temporary.into_os_string()),
                ("LANG".into(), "C".into()),
                ("TERM".into(), "dumb".into()),
            ])),
            stdio: rappct::StdioConfig::Pipe,
            join_job: Some(rappct::JobLimits {
                kill_on_job_close: true,
                ..Default::default()
            }),
            startup_timeout: None,
            ..Default::default()
        };
        let mut child = rappct::launch_in_container_with_io(&capabilities, &options)
            .map_err(|error| err(format!("{mode}: LPAC preparation with pipes: {error:?}")))?;
        drop(child.stdin.take());
        let stdout = capture(child.stdout.take(), "stdout")?;
        let stderr = capture(child.stderr.take(), "stderr")?;
        let result = match child.wait(Some(Duration::from_secs(20))) {
            Ok(code) => Ok(code),
            Err(error) => {
                child.terminate(125).map_err(err)?;
                Err(err(format!("{mode}: LPAC wait failed: {error:?}")))
            }
        };
        // Close the owned command job before joining pipe readers, so a
        // descendant cannot retain a writer after the command has finished.
        drop(child.job_guard.take());
        let stdout = stdout.join().map_err(|_| err("stdout reader panicked"))??;
        let stderr = stderr.join().map_err(|_| err("stderr reader panicked"))??;
        fs::write(root.join(format!("task/stdout-{mode}.txt")), &stdout)?;
        fs::write(root.join(format!("task/stderr-{mode}.txt")), &stderr)?;
        let code = result?;
        let diagnostic = format!(
            "exit={code:#x}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        if mode == "control" {
            println!("control child: {diagnostic}");
        }
        if code != 0 {
            return Err(err(format!("{mode}: LPAC command failed: {diagnostic}")));
        }
        Ok(())
    }

    fn capture(
        pipe: Option<fs::File>,
        name: &'static str,
    ) -> io::Result<std::thread::JoinHandle<io::Result<Vec<u8>>>> {
        let mut pipe = pipe.ok_or_else(|| err(format!("missing prepared {name} pipe")))?;
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                use io::Read as _;
                const LIMIT: usize = 16 * 1024;
                let mut retained = Vec::with_capacity(LIMIT);
                let mut buffer = [0; 4096];
                loop {
                    let count = match pipe.read(&mut buffer) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        result => result?,
                    };
                    if count == 0 {
                        return Ok(retained);
                    }
                    let keep = count.min(LIMIT.saturating_sub(retained.len()));
                    retained.extend_from_slice(&buffer[..keep]);
                }
            })
    }

    fn assert_results(root: &Path, mode: &str, expected: &str) -> io::Result<()> {
        let task = root.join("task");
        for name in [
            "stage", "task", "public", "os", "outside", "stdout", "stderr",
        ] {
            let extension = if name == "os" { "bin" } else { "txt" };
            let file = format!("{name}-{mode}.{extension}");
            match fs::metadata(task.join(&file)) {
                Ok(metadata) => println!("artifact {file}: {} bytes", metadata.len()),
                Err(error) => println!("artifact {file}: {error}"),
            }
        }
        let read = |name: &str| {
            let file = format!("{name}-{mode}.txt");
            fs::read_to_string(task.join(&file))
                .map_err(|error| err(format!("artifact {file}: {error}")))
        };
        let stage = read("stage")?;
        println!("{mode} stages: {stage:?}");
        let image = format!("os-{mode}.bin");
        if read("task")?.trim() != "task-written"
            || read("public")?.trim() != "public-input"
            || fs::read(task.join(&image))
                .map_err(|error| err(format!("artifact {image}: {error}")))?
                .is_empty()
            || read("outside")?.trim() != expected
            || !stage.lines().any(|line| line.trim() == "completed")
        {
            return Err(err(format!("{mode}: fixture result mismatch")));
        }
        Ok(())
    }

    fn launch_restricted_helper(root: &Path, name: &str, package: &str) -> io::Result<()> {
        use firehazard::{job, process, token};
        let host =
            firehazard::open_process_token(firehazard::get_current_process(), token::ALL_ACCESS)
                .map_err(err)?;
        let sid = firehazard::convert_string_sid_to_sid_a(package).map_err(err)?;
        let filtered = firehazard::create_restricted_token(
            &host,
            token::DISABLE_MAX_PRIVILEGE | token::WRITE_RESTRICTED,
            None,
            None,
            Some(&[firehazard::sid::AndAttributes::new(&sid, None)]),
        )
        .map_err(err)?;
        println!(
            "filtered host: restricting SID count={}",
            filtered.restricted_sids().map_err(err)?.group_count()
        );
        let job = firehazard::create_job_object_w(None, ()).map_err(err)?;
        let limits = job::object::ExtendedLimitInformation {
            basic_limit_information: job::object::BasicLimitInformation {
                limit_flags: job::object::limit::KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        firehazard::set_information_job_object(&job, limits).map_err(err)?;
        let image = std::env::current_exe()?;
        let mut command: Vec<u16> = format!(
            "\"{}\" --launcher \"{}\" {name}\0",
            image.display(),
            root.display()
        )
        .encode_utf16()
        .collect();
        let jobs = [job::Handle::from(&job)];
        let mut attributes =
            process::ThreadAttributeList::with_attribute_capacity(1).map_err(err)?;
        attributes
            .update(process::ThreadAttributeRef::job_list(&jobs))
            .map_err(err)?;
        let startup = process::StartupInfoExW {
            attribute_list: Some(attributes),
            ..Default::default()
        };
        let child = firehazard::create_process_as_user_w(
            &filtered,
            (),
            Some(&mut command),
            None,
            None,
            false,
            process::CREATE_SUSPENDED
                | process::CREATE_NO_WINDOW
                | process::EXTENDED_STARTUPINFO_PRESENT,
            process::environment::Inherit,
            (),
            &startup,
        )
        .map_err(err)?;
        // The original process handle pins this PID across the safe OpenProcess.
        let observer = match winsafe::HPROCESS::OpenProcess(
            winsafe::co::PROCESS::SYNCHRONIZE,
            false,
            child.process_id,
        ) {
            Ok(observer) => observer,
            Err(error) => {
                firehazard::terminate_job_object(&job, 125).map_err(err)?;
                firehazard::wait_for_process(&child.process).map_err(err)?;
                return Err(err(error));
            }
        };
        if let Err(error) = firehazard::resume_thread(&child.thread) {
            firehazard::terminate_job_object(&job, 125).map_err(err)?;
            firehazard::wait_for_process(&child.process).map_err(err)?;
            return Err(err(error));
        }
        match observer.WaitForSingleObject(Some(30_000)) {
            Ok(winsafe::co::WAIT::OBJECT_0) => {}
            result => {
                firehazard::terminate_job_object(&job, 125).map_err(err)?;
                firehazard::wait_for_process(&child.process).map_err(err)?;
                return Err(err(format!("filtered launcher wait failed: {result:?}")));
            }
        }
        let code = firehazard::get_exit_code_process(&child.process).map_err(err)?;
        if code != 0 {
            let detail = fs::read_to_string(root.join("task/launcher-error.txt"))
                .unwrap_or_else(|_| "no helper error report".to_owned());
            return Err(err(format!(
                "filtered trusted launcher exited {code:#x}: {detail}"
            )));
        }
        Ok(())
    }
}
