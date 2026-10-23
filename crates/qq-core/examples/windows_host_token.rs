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
        if arguments.get(1).is_some_and(|value| value == "--payload") {
            return payload(Path::new(&arguments[2]), &arguments[3], &arguments[4]);
        }
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
        let private = root.join("private.txt");
        fs::write(&private, b"private-canary")?;
        let aap = root.join("aap-only.txt");
        fs::write(&aap, b"ordinary-appcontainer-input")?;
        let name = format!("qq.host-token-proof.{}", std::process::id());
        let profile =
            rappct::AppContainerProfile::ensure(&name, &name, Some("QQ owned host token proof"))
                .map_err(err)?;
        let outcome = (|| {
            let user = win_custody::current_user_sid_string()?;
            // Keep the LPAC package identity separate from the ordinary
            // NT-authority group SID used by the host's write-only restriction.
            let (groups, _capabilities) =
                firehazard_sid::derive_capability_sids_from_name(format!("{name}.write").as_str())
                    .map_err(|error| {
                        admission_error("DeriveCapabilitySidsFromName(task group)", error)
                    })?;
            if groups.len() != 1 {
                return Err(err(format!(
                    "expected one task group SID, got {}",
                    groups.len()
                )));
            }
            let write_sid = firehazard_sid::convert_sid_to_string_sid_a(&groups[0])
                .ok_or_else(|| err("ConvertSidToStringSidA(task group): missing SID"))?
                .to_string();
            if !write_sid.starts_with("S-1-5-") || write_sid == profile.sid.as_string() {
                return Err(err(
                    "task write SID must have NT authority and differ from the package",
                ));
            }
            println!(
                "task write group SID={write_sid}; LPAC package SID={}",
                profile.sid
            );
            // All ACL changes are confined to these freshly created objects.
            set_acl(
                root,
                &format!("D:P(A;OICI;FA;;;{user})(A;OICI;FRFX;;;S-1-15-2-2)(A;;FRFX;;;S-1-15-2-1)"),
            )?;
            set_acl(
                &workspace,
                &format!(
                    "D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;{})(A;OICI;FA;;;{write_sid})",
                    profile.sid
                ),
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
            set_acl(&private, &format!("D:P(A;;FA;;;{user})"))?;
            set_acl(&aap, &format!("D:P(A;;FA;;;{user})(A;;FR;;;S-1-15-2-1)"))?;
            let payload = workspace.join("owned-payload.exe");
            fs::copy(std::env::current_exe()?, &payload)?;
            set_acl(
                &payload,
                &format!("D:P(A;;FA;;;{user})(A;;FRFX;;;{})", profile.sid),
            )?;
            fs::write(workspace.join("expected-write-sid.txt"), &write_sid)?;
            // Match the established native fixture: ordinary AppContainer must
            // read this exact AAP-only input before LPAC denial is meaningful.
            launch_lpac(root, &name, "ordinary")?;
            if fs::read(workspace.join("aap-ordinary.bin"))? != fs::read(&aap)? {
                return Err(err(
                    "ordinary AppContainer did not read the AAP-only canary",
                ));
            }
            let ordinary: serde_json::Value =
                serde_json::from_slice(&fs::read(workspace.join("token-ordinary.json"))?)?;
            if ordinary["is_appcontainer"] != true
                || ordinary["package_matches"] != true
                || ordinary["task_sid_present"] != false
            {
                return Err(err("ordinary AppContainer token identity mismatch"));
            }
            println!("ordinary AppContainer: exact AAP-only bytes readable");
            // Establish that ordinary LPAC really can perform the canary write.
            launch_lpac(root, &name, "control")?;
            assert_results(root, "control", "allowed")?;
            if fs::read_to_string(&outside)?.trim() != "outside-written" {
                return Err(err(
                    "ordinary LPAC control did not change the ARAP-writable canary",
                ));
            }
            println!(
                "control: task write, OS/public reads, private-read denial and ARAP outside write passed"
            );
            fs::write(&outside, b"original")?;
            // Controls have joined their pipe readers before this synchronous
            // process-station selection interval begins.
            fs::write(workspace.join("private-station.txt"), &name)?;
            fs::write(workspace.join("private-desktop.txt"), "Task")?;
            let ui = PrivateUi::create(&name, &user, &write_sid, profile.sid.as_string())?;
            let launched = launch_restricted_helper(root, &name, &write_sid, &ui.path);
            if let Err(error) = launched {
                // Failed cleanup is not evidence of job quiescence. Retain
                // private objects until this bounded diagnostic process exits.
                std::mem::forget(ui);
                return Err(error);
            }
            // Successful launch verifies zero active job processes before UI
            // close; field order closes the desktop before its station.
            drop(ui);
            assert_results(root, "filtered", "denied")?;
            if fs::read(&outside)? != b"original" {
                return Err(err("filtered LPAC changed the outside canary"));
            }
            println!(
                "filtered: task write, OS/public reads, private-read denial passed; outside write denied"
            );
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
            .lpac(mode != "ordinary")
            .build()
            .map_err(err)?;
        let windows = std::env::var_os("SystemRoot").ok_or_else(|| err("missing SystemRoot"))?;
        let home = root.join("task/home");
        let temporary = home.join("tmp");
        let options = rappct::LaunchOptions {
            exe: root.join("task/owned-payload.exe"),
            cmdline: Some(format!(
                "owned-payload.exe --payload \"{}\" {mode} {}",
                root.display(),
                package.as_string(),
            )),
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
        let options = if mode == "filtered" {
            let station = fs::read_to_string(root.join("task/private-station.txt"))?;
            let desktop = fs::read_to_string(root.join("task/private-desktop.txt"))?;
            rappct::LaunchOptions {
                desktop: Some(format!("{station}\\{desktop}").into()),
                ..options
            }
        } else {
            options
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

    fn payload(root: &Path, mode: &str, package: &str) -> io::Result<()> {
        use std::{
            os::windows::process::CommandExt as _,
            process::{Command, Stdio},
        };
        let task = root.join("task");
        let token = firehazard::open_process_token(
            firehazard::get_current_process(),
            firehazard::token::QUERY,
        )
        .map_err(err)?;
        let restricted = token.restricted_sids().map_err(err)?;
        let write_sid = fs::read_to_string(task.join("expected-write-sid.txt"))?;
        let expected = firehazard::convert_string_sid_to_sid_a(write_sid.as_str())
            .map_err(|error| admission_error("ConvertStringSidToSid(payload task group)", error))?;
        let task_sid_present = restricted
            .groups()
            .iter()
            .any(|group| firehazard::equal_sid(group.sid, (&expected).into()));
        let app = rappct::token::query_current_process_token().map_err(err)?;
        let access_flags = token.access_information().map_err(err)?.Flags;
        let witness = serde_json::json!({
            "access_information_flags": access_flags,
            "restricted_sid_count": restricted.group_count(),
            "task_sid_present": task_sid_present,
            "task_group_sid": write_sid,
            "is_appcontainer": app.is_appcontainer,
            "is_lpac": app.is_lpac,
            "package_matches": app.package_sid.as_ref().is_some_and(|sid| sid.as_string() == package),
        });
        println!("{mode} payload token: {witness}");
        fs::write(task.join(format!("token-{mode}.json")), witness.to_string())?;
        let aap = fs::read(root.join("aap-only.txt"));
        println!("{mode} AAP-only read: {aap:?}");
        if mode == "ordinary" {
            fs::write(task.join("aap-ordinary.bin"), aap?)?;
            return Ok(());
        }
        let aap_denied = aap
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
        fs::write(task.join(format!("task-{mode}.txt")), b"task-written")?;
        let public = fs::read(root.join("public.txt"))?;
        fs::write(task.join(format!("public-{mode}.txt")), public)?;
        let private = fs::read(root.join("private.txt"));
        let private_denied = private
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
        println!("{mode} private read: {private:?}");
        let nul = fs::OpenOptions::new().read(true).write(true).open("NUL");
        println!("{mode} independent NUL compatibility: {nul:?}");
        let windows = std::env::var_os("SystemRoot").ok_or_else(|| err("missing SystemRoot"))?;
        let image = Path::new(&windows).join("System32").join("cmd.exe");
        let input = task.join(format!("copy-input-{mode}.txt"));
        fs::write(&input, b"")?;
        let mut copy = Command::new(&image);
        copy.stdin(Stdio::from(fs::File::open(input)?)).current_dir(&task).raw_arg(format!(
            "/D /S /C \"copy /b \"{}\" os-{mode}.bin >copy-{mode}.log 2>copy-error-{mode}.log\"",
            image.display(),
        ));
        let copied = copy.output();
        println!("{mode} owned-log COPY: {copied:?}");
        let outside = fs::write(root.join("outside.txt"), b"outside-written");
        let outside_denied = outside
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
        println!("{mode} outside write: {outside:?}");
        let result = serde_json::json!({
            "aap_read_denied": aap_denied,
            "private_read_denied": private_denied,
            "outside_write_allowed": outside.is_ok(),
            "outside_write_denied": outside_denied,
            "copy_status_success": copied.as_ref().is_ok_and(|output| output.status.success()),
            "nul_available": nul.is_ok(),
        });
        fs::write(task.join(format!("result-{mode}.json")), result.to_string())?;
        println!("{mode} payload observations: {result}");
        Ok(())
    }

    fn assert_results(root: &Path, mode: &str, expected: &str) -> io::Result<()> {
        let task = root.join("task");
        let witness: serde_json::Value =
            serde_json::from_slice(&fs::read(task.join(format!("token-{mode}.json")))?)?;
        let result: serde_json::Value =
            serde_json::from_slice(&fs::read(task.join(format!("result-{mode}.json")))?)?;
        println!("{mode} verified token: {witness}; observations: {result}");
        let windows = std::env::var_os("SystemRoot").ok_or_else(|| err("missing SystemRoot"))?;
        let source = fs::read(Path::new(&windows).join("System32").join("cmd.exe"))?;
        let image = fs::read(task.join(format!("os-{mode}.bin")));
        println!("{mode} OS copy bytes: {:?}", image.as_ref().map(Vec::len));
        for name in ["copy", "copy-error", "stdout", "stderr"] {
            let data = fs::read(task.join(format!("{name}-{mode}.log")))
                .or_else(|_| fs::read(task.join(format!("{name}-{mode}.txt"))));
            println!("{mode} {name}: {data:?}");
        }
        if witness["is_appcontainer"] != true
            || witness["package_matches"] != true
            || witness["task_sid_present"] != (mode == "filtered")
            || fs::read(task.join(format!("task-{mode}.txt")))? != b"task-written"
            || fs::read(task.join(format!("public-{mode}.txt")))? != b"public-input"
            || image? != source
            // TokenIsLessPrivilegedAppContainer is reserved; the pinned SDK
            // maps an unsupported query to false. Enforce its defining AAP-only
            // denial with the ordinary-AppContainer positive control above.
            || result["aap_read_denied"] != true
            || result["private_read_denied"] != true
            || result["copy_status_success"] != true
            || result[if expected == "allowed" {
                "outside_write_allowed"
            } else {
                "outside_write_denied"
            }] != true
        {
            return Err(err(format!("{mode}: containment proof failed")));
        }
        Ok(())
    }

    // Diagnostic-only owned UI objects. Fields drop in order: desktop, station.
    struct PrivateUi {
        _desktop: firehazard_sid::desktop::OwnedHandle,
        _station: firehazard_sid::winsta::OwnedHandle,
        path: String,
    }

    fn station_participant() -> firehazard_sid::winsta::AccessRights {
        use firehazard_sid::winsta::*;
        READATTRIBUTES
            | WRITEATTRIBUTES
            | ENUMDESKTOPS
            | ACCESSGLOBALATOMS
            | firehazard_sid::access::READ_CONTROL
    }

    fn desktop_participant() -> firehazard_sid::desktop::AccessRights {
        use firehazard_sid::desktop::*;
        READOBJECTS
            | WRITEOBJECTS
            | CREATEWINDOW
            | CREATEMENU
            | ENUMERATE
            | firehazard_sid::access::READ_CONTROL
    }

    impl PrivateUi {
        fn create(name: &str, user: &str, task: &str, package: &str) -> io::Result<Self> {
            use firehazard_sid as ui;
            let user = ui::convert_string_sid_to_sid_w(user).map_err(err)?;
            let task = ui::convert_string_sid_to_sid_w(task).map_err(err)?;
            let package = ui::convert_string_sid_to_sid_w(package).map_err(err)?;
            let mut station_acl = ui::acl::Builder::new(ui::acl::REVISION);
            let mut desktop_acl = ui::acl::Builder::new(ui::acl::REVISION);
            station_acl
                .add_access_allowed_ace(ui::acl::REVISION, ui::access::GENERIC_ALL, &user)
                .map_err(err)?;
            desktop_acl
                .add_access_allowed_ace(ui::acl::REVISION, ui::access::GENERIC_ALL, &user)
                .map_err(err)?;
            for participant in [&task, &package] {
                station_acl
                    .add_access_allowed_ace(ui::acl::REVISION, station_participant(), participant)
                    .map_err(err)?;
                desktop_acl
                    .add_access_allowed_ace(ui::acl::REVISION, desktop_participant(), participant)
                    .map_err(err)?;
            }
            station_acl.finish().map_err(err)?;
            desktop_acl.finish().map_err(err)?;
            let mut label = ui::acl::Builder::new(ui::acl::REVISION);
            // SYSTEM_MANDATORY_LABEL_NO_WRITE_UP, with a Low object label.
            label
                .add_mandatory_ace(
                    ui::acl::REVISION,
                    Default::default(),
                    1,
                    &ui::sid::integrity::Low,
                )
                .map_err(err)?;
            label.finish().map_err(err)?;
            let station_sd = ui::security::DescriptorBuilder::new()
                .dacl(station_acl.as_acl(), false)
                .map_err(err)?
                .sacl(label.as_acl(), false)
                .map_err(err)?
                .finish();
            let desktop_sd = ui::security::DescriptorBuilder::new()
                .dacl(desktop_acl.as_acl(), false)
                .map_err(err)?
                .sacl(label.as_acl(), false)
                .map_err(err)?
                .finish();
            let station = ui::create_window_station_w(
                name,
                None,
                ui::access::GENERIC_ALL,
                Some(&ui::security::Attributes::new(Some(&station_sd), false)),
            )
            .map_err(|error| admission_error("CreateWindowStationW(private)", error))?;
            let original = ui::open_process_window_station().map_err(err)?;
            ui::set_process_window_station(&station).map_err(err)?;
            let desktop = ui::create_desktop_w(
                "Task",
                (),
                None,
                None,
                ui::access::GENERIC_ALL,
                Some(&ui::security::Attributes::new(Some(&desktop_sd), false)),
            );
            // Always attempt restoration, including CreateDesktop failure.
            let restored = ui::set_process_window_station(&original);
            // This duplicate becomes the process connection on restoration.
            // Retain it until this bounded diagnostic exits; this is NOT a
            // reusable production lifetime design. Host ACLs are unchanged.
            std::mem::forget(original);
            if let Err(error) = restored {
                // Restoration failure leaves the private station connected.
                // Do not invoke Close* on still-connected objects; the bounded
                // diagnostic process releases them on exit and reports failure.
                std::mem::forget(station);
                std::mem::forget(desktop);
                return Err(admission_error("SetProcessWindowStation(restore)", error));
            }
            let desktop =
                desktop.map_err(|error| admission_error("CreateDesktopW(private)", error))?;
            Ok(Self {
                _desktop: desktop,
                _station: station,
                path: format!("{name}\\Task"),
            })
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("{operation}: {source}")]
    struct AdmissionError {
        operation: &'static str,
        #[source]
        source: io::Error,
    }

    fn admission_error(operation: &'static str, source: impl Into<io::Error>) -> io::Error {
        let source = source.into();
        io::Error::new(source.kind(), AdmissionError { operation, source })
    }

    fn launch_restricted_helper(
        root: &Path,
        name: &str,
        write_sid: &str,
        desktop_path: &str,
    ) -> io::Result<()> {
        use firehazard::{job, process, token};
        let host =
            firehazard::open_process_token(firehazard::get_current_process(), token::ALL_ACCESS)
                .map_err(|error| admission_error("OpenProcessToken(host)", error))?;
        let sid = firehazard::convert_string_sid_to_sid_a(write_sid)
            .map_err(|error| admission_error("ConvertStringSidToSid(task)", error))?;
        let flags = token::DISABLE_MAX_PRIVILEGE | token::WRITE_RESTRICTED;
        let restricting = [firehazard::sid::AndAttributes::new(&sid, None)];
        println!(
            "CreateRestrictedToken inputs: flags={:#x}, disabled_sids=0, deleted_privileges=0, restricting_sids={}, restricting_attributes={:#x}",
            u32::from(flags),
            restricting.len(),
            u32::from(restricting[0].attributes),
        );
        let filtered =
            firehazard::create_restricted_token(&host, flags, None, None, Some(&restricting))
                .map_err(|error| admission_error("CreateRestrictedToken", error))?;
        let restricting = filtered.restricted_sids().map_err(|error| {
            admission_error("GetTokenInformation(filtered, TokenRestrictedSids)", error)
        })?;
        println!(
            "filtered host: restricting SID count={}",
            restricting.group_count()
        );
        let job = firehazard::create_job_object_w(None, ())
            .map_err(|error| admission_error("CreateJobObjectW", error))?;
        let limits = job::object::ExtendedLimitInformation {
            basic_limit_information: job::object::BasicLimitInformation {
                limit_flags: job::object::limit::KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        firehazard::set_information_job_object(&job, limits)
            .map_err(|error| admission_error("SetInformationJobObject", error))?;
        let image = std::env::current_exe()
            .map_err(|error| admission_error("current_exe(helper)", error))?;
        let mut command: Vec<u16> = format!(
            "\"{}\" --launcher \"{}\" {name}\0",
            image.display(),
            root.display()
        )
        .encode_utf16()
        .collect();
        let jobs = [job::Handle::from(&job)];
        let mut attributes = process::ThreadAttributeList::with_attribute_capacity(1)
            .map_err(|error| admission_error("InitializeProcThreadAttributeList", error))?;
        attributes
            .update(process::ThreadAttributeRef::job_list(&jobs))
            .map_err(|error| admission_error("UpdateProcThreadAttribute(job list)", error))?;
        let desktop: Vec<u16> = desktop_path.encode_utf16().chain(Some(0)).collect();
        let mut startup = process::StartupInfoExW {
            attribute_list: Some(attributes),
            ..Default::default()
        };
        startup.startup_info.desktop =
            Some(firehazard::abistr::CStrNonNull::from_units_with_nul(&desktop).map_err(err)?);
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
        .map_err(|error| admission_error("CreateProcessAsUserW(filtered helper)", error))?;
        // The original process handle pins this PID across the safe OpenProcess.
        let observer = match winsafe::HPROCESS::OpenProcess(
            winsafe::co::PROCESS::SYNCHRONIZE,
            false,
            child.process_id,
        ) {
            Ok(observer) => observer,
            Err(error) => {
                firehazard::terminate_job_object(&job, 125)
                    .map_err(|error| admission_error("TerminateJobObject(cleanup)", error))?;
                firehazard::wait_for_process(&child.process)
                    .map_err(|error| admission_error("wait_for_process(cleanup)", error))?;
                return Err(admission_error(
                    "OpenProcess(observer)",
                    io::Error::other(error),
                ));
            }
        };
        if let Err(error) = firehazard::resume_thread(&child.thread) {
            firehazard::terminate_job_object(&job, 125)
                .map_err(|error| admission_error("TerminateJobObject(cleanup)", error))?;
            firehazard::wait_for_process(&child.process)
                .map_err(|error| admission_error("wait_for_process(cleanup)", error))?;
            return Err(admission_error("ResumeThread(filtered helper)", error));
        }
        match observer.WaitForSingleObject(Some(30_000)) {
            Ok(winsafe::co::WAIT::OBJECT_0) => {}
            result => {
                firehazard::terminate_job_object(&job, 125)
                    .map_err(|error| admission_error("TerminateJobObject(cleanup)", error))?;
                firehazard::wait_for_process(&child.process)
                    .map_err(|error| admission_error("wait_for_process(cleanup)", error))?;
                return Err(match result {
                    Err(error) => admission_error(
                        "WaitForSingleObject(filtered helper)",
                        io::Error::other(error),
                    ),
                    Ok(status) => err(format!(
                        "WaitForSingleObject(filtered helper): unexpected status {status:?}"
                    )),
                });
            }
        }
        let code = firehazard::get_exit_code_process(&child.process)
            .map_err(|error| admission_error("GetExitCodeProcess(filtered helper)", error))?;
        if code != 0 {
            let detail = fs::read_to_string(root.join("task/launcher-error.txt"))
                .unwrap_or_else(|_| "no helper error report".to_owned());
            return Err(err(format!(
                "filtered trusted launcher exited {code:#x}: {detail}"
            )));
        }
        let accounting = firehazard::query_information_job_object::<
            job::object::BasicAccountingInformation,
        >(&job)
        .map_err(|error| {
            admission_error("QueryInformationJobObject(final active processes)", error)
        })?;
        if accounting.active_processes != 0 {
            firehazard::terminate_job_object(&job, 125).map_err(|error| {
                admission_error("TerminateJobObject(non-quiescent proof)", error)
            })?;
            return Err(err(format!(
                "helper job still has {} active processes",
                accounting.active_processes
            )));
        }
        println!("filtered helper job: zero active processes before private UI cleanup");
        Ok(())
    }
}
