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
        if arguments
            .get(1)
            .is_none_or(|value| value != "--ordinary-broker")
        {
            return launch_ordinary_broker();
        }
        verify_ordinary_broker("before full proof")?;
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
            // Resolve Userenv-backed identities before applying the host write
            // restriction. The helper receives trusted data, not new authority.
            let package = rappct::derive_sid_from_name(&name).map_err(err)?;
            if package != profile.sid {
                return Err(err("prepared package differs from the created profile"));
            }
            let baseline = rappct::SecurityCapabilitiesBuilder::new(&package)
                .with_named(&["registryRead"])
                .lpac(true)
                .build()
                .map_err(err)?;
            let identities = serde_json::json!({
                "package": baseline.package.as_string(),
                "caps": baseline.caps.iter().map(|cap| serde_json::json!({
                    "sid": cap.sid_sddl, "attributes": cap.attributes,
                })).collect::<Vec<_>>(),
                "lpac": baseline.lpac,
            });
            let identity_path = root.join("launch-identities.json");
            fs::write(&identity_path, serde_json::to_vec(&identities)?)?;
            // Outside the task write tree, with no package/public/task ACE.
            set_acl(&identity_path, &format!("D:P(A;;FA;;;{user})"))?;
            if read_identities(root)? != baseline {
                return Err(err(
                    "prepared identity roundtrip changed SID bytes or attributes",
                ));
            }
            println!("parent prepared exact baseline identities: {identities}");
            // Match the established native fixture: ordinary AppContainer must
            // read this exact AAP-only input before LPAC denial is meaningful.
            launch_lpac(root, "ordinary")?;
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
            launch_lpac(root, "control")?;
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
            // Each payload uses its own desktop in the existing station.
            let ui = PrivateUi::create(&name, &user, &write_sid, profile.sid.as_string())?;
            let launched = launch_restricted_payload(root, &write_sid, &ui.path);
            if let Err(error) = launched {
                // Failed cleanup is not evidence of job quiescence. Retain
                // private objects until this bounded diagnostic process exits.
                std::mem::forget(ui);
                return Err(error);
            }
            // Successful launch verifies zero active job processes before close.
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

    fn read_identities(root: &Path) -> io::Result<rappct::SecurityCapabilities> {
        let bytes = fs::read(root.join("launch-identities.json"))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let package = value["package"]
            .as_str()
            .ok_or_else(|| err("missing package SID"))?;
        let package = rappct::AppContainerSid::try_from_sddl(package).map_err(err)?;
        let rows = value["caps"]
            .as_array()
            .ok_or_else(|| err("missing capability identities"))?;
        if rows.is_empty() || rows.len() > 8 || value["lpac"] != true {
            return Err(err("invalid prepared capability manifest"));
        }
        let caps = rows
            .iter()
            .map(|row| {
                let sid = row["sid"]
                    .as_str()
                    .ok_or_else(|| err("missing capability SID"))?;
                // Advapi SID parsing requires no Userenv/profile derivation.
                let _validated = firehazard::convert_string_sid_to_sid_a(sid).map_err(err)?;
                let attributes = row["attributes"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| err("invalid capability attributes"))?;
                Ok(rappct::sid::SidAndAttributes {
                    sid_sddl: sid.to_owned(),
                    attributes,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(rappct::SecurityCapabilities {
            package,
            caps,
            lpac: true,
        })
    }

    fn launch_lpac(root: &Path, mode: &str) -> io::Result<()> {
        let user = win_custody::current_user_sid_string()?;
        let capabilities = read_identities(root)?;
        let name = format!("qq.control-ui.{mode}.{}", std::process::id());
        // The outer ordinary broker's UI grants only its user. Each control
        // needs its own package-authorized UI rather than inheriting that UI.
        // Keep these owned objects through the bounded broker process lifetime.
        let ui = std::mem::ManuallyDrop::new(PrivateUi::create(
            &name,
            &user,
            &user,
            capabilities.package.as_string(),
        )?);
        launch_lpac_as(root, mode, None, &ui.path)
    }

    fn launch_lpac_as(
        root: &Path,
        mode: &str,
        primary: Option<std::os::windows::io::BorrowedHandle<'_>>,
        desktop: &str,
    ) -> io::Result<()> {
        let token = firehazard::open_process_token(
            firehazard::get_current_process(),
            firehazard::token::QUERY,
        )
        .map_err(err)?;
        let count = token.restricted_sids().map_err(err)?.group_count();
        if mode == "control" {
            println!("control launcher: restricting SID count={count}");
        } else {
            fs::write(
                root.join("task/broker-restricting-count.txt"),
                count.to_string(),
            )?;
        }
        if primary.is_some() {
            println!(
                "filtered broker caller restricting SID count={count}; explicit task token supplied"
            );
        }
        let mut capabilities = read_identities(root)?;
        capabilities.lpac = mode != "ordinary";
        let package = &capabilities.package;
        fs::write(
            root.join(format!("task/used-identities-{mode}.json")),
            serde_json::to_vec(&serde_json::json!({
                "package": package.as_string(),
                "caps": capabilities.caps.iter().map(|cap| serde_json::json!({
                    "sid": cap.sid_sddl, "attributes": cap.attributes,
                })).collect::<Vec<_>>(),
                "lpac": capabilities.lpac,
            }))?,
        )?;
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
            desktop: Some(desktop.into()),
            ..Default::default()
        };
        let options = if let Some(token) = primary {
            options.try_with_primary_token(token).map_err(err)?
        } else {
            options
        };
        let mut child = rappct::launch_in_container_with_io(&capabilities, &options)
            .map_err(|error| err(format!("{mode}: LPAC preparation with pipes: {error:?}")))?;
        drop(child.stdin.take());
        let stdout = capture(child.stdout.take(), "stdout")?;
        let stderr = capture(child.stderr.take(), "stderr")?;
        if primary.is_some() {
            let active = child
                .job_guard
                .as_ref()
                .ok_or_else(|| err("missing birth job guard"))?
                .active_processes()
                .map_err(err)?;
            if active != 1 || root.join("task/task-filtered.txt").exists() {
                return Err(err("payload ran before suspended birth-job admission"));
            }
            println!(
                "filtered birth job: one suspended process; payload artifact absent before release"
            );
            child.resume().map_err(err)?;
            if child.resume().is_ok() {
                return Err(err("payload resumed more than once"));
            }
        }
        let result = match child.wait(Some(Duration::from_secs(20))) {
            Ok(code) => Ok(code),
            Err(error) => {
                child.terminate(125).map_err(err)?;
                Err(err(format!("{mode}: LPAC wait failed: {error:?}")))
            }
        };
        // Close the owned command job before joining pipe readers, so a
        // descendant cannot retain a writer after the command has finished.
        let active = if primary.is_some() {
            Some(
                child
                    .job_guard
                    .as_ref()
                    .ok_or_else(|| err("missing birth job guard"))?
                    .active_processes()
                    .map_err(err)?,
            )
        } else {
            None
        };
        drop(child.job_guard.take());
        let stdout = stdout.join().map_err(|_| err("stdout reader panicked"))??;
        let stderr = stderr.join().map_err(|_| err("stderr reader panicked"))??;
        fs::write(root.join(format!("task/stdout-{mode}.txt")), &stdout)?;
        fs::write(root.join(format!("task/stderr-{mode}.txt")), &stderr)?;
        let code = result?;
        if let Some(active) = active {
            if active != 0 {
                return Err(err(format!(
                    "birth job still has {active} active processes"
                )));
            }
            println!("filtered birth job: zero active processes before private UI cleanup");
        }
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
        pipe: Option<impl io::Read + Send + 'static>,
        name: &'static str,
    ) -> io::Result<std::thread::JoinHandle<io::Result<Vec<u8>>>> {
        let mut pipe = pipe.ok_or_else(|| err(format!("missing prepared {name} pipe")))?;
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
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
        let baseline: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("launch-identities.json"))?)?;
        let used: serde_json::Value = serde_json::from_slice(&fs::read(
            task.join(format!("used-identities-{mode}.json")),
        )?)?;
        if used != baseline {
            return Err(err("launcher changed parent-prepared identities"));
        }
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

    // The desktop belongs to the existing station; no host UI ACL is edited.
    struct PrivateUi {
        _desktop: firehazard_sid::desktop::OwnedHandle,
        path: String,
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
            let mut desktop_acl = ui::acl::Builder::new(ui::acl::REVISION);
            desktop_acl
                .add_access_allowed_ace(ui::acl::REVISION, ui::access::GENERIC_ALL, &user)
                .map_err(err)?;
            // CreateProcessAsUser requires full access for the trusted helper.
            // Its task SID is granted this only on these fresh noninteractive
            // objects; the LPAC package keeps the narrower participant rights.
            desktop_acl
                .add_access_allowed_ace(ui::acl::REVISION, ui::access::GENERIC_ALL, &task)
                .map_err(err)?;
            desktop_acl
                .add_access_allowed_ace(ui::acl::REVISION, desktop_participant(), &package)
                .map_err(err)?;
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
            let desktop_sd = ui::security::DescriptorBuilder::new()
                .dacl(desktop_acl.as_acl(), false)
                .map_err(err)?
                .sacl(label.as_acl(), false)
                .map_err(err)?
                .finish();
            // CreateDesktop associates the object with the current station.
            // A desktop-only STARTUPINFO name selects it in the inherited station.
            let desktop = ui::create_desktop_w(
                name,
                (),
                None,
                None,
                ui::access::GENERIC_ALL,
                Some(&ui::security::Attributes::new(Some(&desktop_sd), false)),
            );
            let desktop =
                desktop.map_err(|error| admission_error("CreateDesktopW(private)", error))?;
            Ok(Self {
                _desktop: desktop,
                path: name.to_owned(),
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

    fn privileged_group(sid: &str) -> bool {
        if let Some(rid) = sid.strip_prefix("S-1-5-32-") {
            return matches!(
                rid,
                "544"
                    | "547"
                    | "548"
                    | "549"
                    | "550"
                    | "551"
                    | "552"
                    | "556"
                    | "569"
                    | "578"
                    | "580"
                    | "582"
            );
        }
        sid.starts_with("S-1-5-21-")
            && matches!(
                sid.rsplit('-').next(),
                Some("512" | "518" | "519" | "520" | "526" | "527")
            )
    }

    fn verify_ordinary_broker(stage: &str) -> io::Result<()> {
        use firehazard_sid as fh;
        let token =
            fh::open_process_token(fh::get_current_process(), fh::token::QUERY).map_err(err)?;
        let integrity = token.integrity_level().map_err(err)?;
        if !fh::equal_sid(integrity.label().sid, (&fh::sid::integrity::Medium).into()) {
            return Err(err(
                "broker process primary token must have Medium integrity",
            ));
        }
        let groups = token.groups().map_err(err)?;
        let mut administrators = false;
        let mut disabled = 0;
        for group in groups.groups() {
            let sid = fh::convert_sid_to_string_sid_a(&group.sid)
                .ok_or_else(|| err("group SID was null"))?
                .to_string();
            if privileged_group(&sid) {
                if group.attributes & fh::sid::group::USE_FOR_DENY_ONLY == Default::default()
                    || group.attributes & fh::sid::group::ENABLED != Default::default()
                {
                    return Err(err(format!(
                        "broker privileged group must be deny-only: {sid}"
                    )));
                }
                administrators |= sid == "S-1-5-32-544";
                disabled += 1;
            }
        }
        if !administrators {
            return Err(err(
                "qualification requires Administrators deny-only witness",
            ));
        }
        let notify = fh::privilege::name::CHANGE_NOTIFY
            .lookup_luid()
            .map_err(err)?;
        let privileges = token.privileges().map_err(err)?;
        if privileges.privileges().len() != 1
            || privileges.privileges()[0].luid != notify
            || privileges.privileges()[0].attributes & fh::privilege::ENABLED == Default::default()
        {
            return Err(err(
                "broker must have only originally enabled ChangeNotify privilege; AssignPrimary/IncreaseQuota absent",
            ));
        }
        if token.restricted_sids().map_err(err)?.group_count() != 0 {
            return Err(err("ordinary broker must have no restricting SID list"));
        }
        println!(
            "ordinary broker actual primary token ({stage}): Medium; {disabled} admin/operator groups deny-only; only enabled ChangeNotify; AssignPrimary/IncreaseQuota absent; zero restrictors"
        );
        Ok(())
    }

    fn read_captured(mut pipe: impl io::Read) -> io::Result<Vec<u8>> {
        let mut retained = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = match pipe.read(&mut buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // Anonymous Windows pipes signal normal writer closure with error 109.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(retained),
                result => result?,
            };
            if count == 0 {
                return Ok(retained);
            }
            let keep = count.min((16 * 1024_usize).saturating_sub(retained.len()));
            retained.extend_from_slice(&buffer[..keep]);
        }
    }

    fn capture_bounded(
        pipe: impl io::Read + Send + 'static,
        name: &str,
    ) -> io::Result<std::sync::mpsc::Receiver<io::Result<Vec<u8>>>> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let _ = sender.send(read_captured(pipe));
            })?;
        Ok(receiver)
    }

    fn launch_ordinary_broker() -> io::Result<()> {
        use fh::{job, process};
        use firehazard as fh;
        let host = fh::open_process_token(fh::get_current_process(), fh::token::ALL_ACCESS)
            .map_err(err)?;
        let notify = fh::privilege::name::CHANGE_NOTIFY.luid();
        let privileges = host.privileges().map_err(err)?;
        if !privileges.privileges().iter().any(|p| {
            p.luid == notify && p.attributes & fh::privilege::ENABLED != Default::default()
        }) {
            return Err(err("original host ChangeNotify must already be enabled"));
        }
        let groups = host.groups().map_err(err)?;
        let disabled: Vec<_> = groups
            .groups()
            .iter()
            .filter(|g| {
                fh::convert_sid_to_string_sid_a(&g.sid)
                    .map(|s| privileged_group(&s.to_string()))
                    .unwrap_or(true)
            })
            .copied()
            .collect();
        let deleted: Vec<_> = privileges
            .privileges()
            .iter()
            .filter(|p| p.luid != notify)
            .copied()
            .collect();
        let token = fh::create_restricted_token(&host, None, Some(&disabled), Some(&deleted), None)
            .map_err(err)?;
        token
            .set_integrity_level(fh::sid::AndAttributes::new(
                &fh::sid::integrity::Medium,
                None,
            ))
            .map_err(err)?;
        let token_user = token.user().map_err(err)?;
        let user_sid = fh::convert_sid_to_string_sid_a(&token_user.user().sid)
            .ok_or_else(|| err("ordinary broker token user SID was null"))?
            .to_string();
        let old_default = token.default_dacl().map_err(err)?;
        let old_acl = old_default.default_dacl();
        let original_count = old_acl.ace_count();
        println!(
            "ordinary broker original new-object default DACL: {:?}",
            old_acl.aces().collect::<Vec<_>>()
        );
        let mut acl = fh::acl::Builder::new(fh::acl::REVISION_DS);
        acl.add_acl(fh::acl::REVISION_DS, 0, old_acl).map_err(err)?;
        acl.add_access_allowed_ace(
            fh::acl::REVISION_DS,
            fh::access::GENERIC_ALL.into(),
            token_user.user().sid,
        )
        .map_err(err)?;
        acl.finish().map_err(err)?;
        token.set_default_dacl(&mut acl).map_err(err)?;
        let readback = token.default_dacl().map_err(err)?;
        if readback.default_dacl().ace_count() != original_count + 1 {
            return Err(err(
                "ordinary broker new-object default DACL count mismatch",
            ));
        }
        println!(
            "ordinary broker new-object default DACL: preserved {original_count} ACEs; appended actual user GENERIC_ALL: {:?}",
            readback.default_dacl().aces().last()
        );
        // All constructor roles use the same actual user SID: no package/task
        // identity receives rights on this outer broker's fresh private UI.
        // This corrects unmeasured desktop startup preconditions; it does not
        // identify which hosted-runner object caused STATUS_DLL_INIT_FAILED.
        let ui_name = format!("qq.ordinary-broker.{}", std::process::id());
        let ordinary_ui = std::mem::ManuallyDrop::new(PrivateUi::create(
            &ui_name, &user_sid, &user_sid, &user_sid,
        )?);
        let job = fh::create_job_object_w(None, ()).map_err(err)?;
        fh::set_information_job_object(
            &job,
            job::object::ExtendedLimitInformation {
                basic_limit_information: job::object::BasicLimitInformation {
                    limit_flags: job::object::limit::KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .map_err(err)?;
        let inherit = fh::security::Attributes::new(None, true);
        let (stdin, stdin_writer) = fh::create_pipe(Some(&inherit), 0).map_err(err)?;
        let (stdout_reader, stdout) = fh::create_pipe(Some(&inherit), 0).map_err(err)?;
        let (stderr_reader, stderr) = fh::create_pipe(Some(&inherit), 0).map_err(err)?;
        drop(stdin_writer);
        fh::set_handle_information(&stdout_reader, fh::handle::FLAG_INHERIT, None).map_err(err)?;
        fh::set_handle_information(&stderr_reader, fh::handle::FLAG_INHERIT, None).map_err(err)?;
        let handles = [
            fh::handle::Borrowed::from(&stdin),
            fh::handle::Borrowed::from(&stdout),
            fh::handle::Borrowed::from(&stderr),
        ];
        let jobs = [job::Handle::from(&job)];
        let mut attributes =
            process::ThreadAttributeList::with_attribute_capacity(2).map_err(err)?;
        attributes
            .update(process::ThreadAttributeRef::job_list(&jobs))
            .map_err(err)?;
        attributes
            .update(process::ThreadAttributeRef::handle_list(&handles))
            .map_err(err)?;
        let desktop: Vec<u16> = ordinary_ui.path.encode_utf16().chain(Some(0)).collect();
        let mut startup = process::StartupInfoExW {
            attribute_list: Some(attributes),
            ..Default::default()
        };
        startup.startup_info.desktop =
            Some(fh::abistr::CStrNonNull::from_units_with_nul(&desktop).map_err(err)?);
        startup.startup_info.flags = 0x100; // STARTF_USESTDHANDLES
        startup.startup_info.std_input = Some((&stdin).into());
        startup.startup_info.std_output = Some((&stdout).into());
        startup.startup_info.std_error = Some((&stderr).into());
        let image = std::env::current_exe()?;
        let mut command: Vec<u16> = format!("\"{}\" --ordinary-broker\0", image.display())
            .encode_utf16()
            .collect();
        let captured_stdout = capture_bounded(stdout_reader, "ordinary-broker-stdout")?;
        let captured_stderr = capture_bounded(stderr_reader, "ordinary-broker-stderr")?;
        let child = fh::create_process_as_user_w(
            &token,
            (),
            Some(&mut command),
            None,
            None,
            true,
            process::CREATE_SUSPENDED
                | process::CREATE_NO_WINDOW
                | process::EXTENDED_STARTUPINFO_PRESENT,
            process::environment::Inherit,
            (),
            &startup,
        )
        .map_err(|e| admission_error("CreateProcessAsUserW(ordinary broker wrapper)", e))?;
        drop(startup);
        drop(stdin);
        drop(stdout);
        drop(stderr);
        let observer = match winsafe::HPROCESS::OpenProcess(
            winsafe::co::PROCESS::SYNCHRONIZE,
            false,
            child.process_id,
        ) {
            Ok(observer) => observer,
            Err(error) => {
                fh::terminate_job_object(&job, 125).map_err(err)?;
                return Err(err(format!(
                    "ordinary broker observer unavailable; termination requested, cleanup unconfirmed: {error}"
                )));
            }
        };
        let result = (|| {
            if fh::resume_thread(&child.thread).map_err(err)? != 1 {
                return Err(err("ordinary broker suspension count was not exactly one"));
            }
            match observer
                .WaitForSingleObject(Some(60_000))
                .map_err(io::Error::other)?
            {
                winsafe::co::WAIT::OBJECT_0 => {}
                status => return Err(err(format!("ordinary broker wait: {status:?}"))),
            }
            let code = fh::get_exit_code_process(&child.process).map_err(err)?;
            if code != 0 {
                return Err(err(format!("ordinary broker exit {code:#x}")));
            }
            let accounting =
                fh::query_information_job_object::<job::object::BasicAccountingInformation>(&job)
                    .map_err(err)?;
            if accounting.active_processes != 0 {
                return Err(err("ordinary broker job not quiescent"));
            }
            Ok(())
        })();
        if result.is_err() {
            fh::terminate_job_object(&job, 125).map_err(err)?;
            if observer
                .WaitForSingleObject(Some(5_000))
                .map_err(io::Error::other)?
                != winsafe::co::WAIT::OBJECT_0
            {
                return Err(err(
                    "ordinary broker cleanup unconfirmed after bounded wait",
                ));
            }
        }
        drop(job);
        if result.is_ok() {
            // Success above includes the entire outer job's zero-active witness.
            drop(std::mem::ManuallyDrop::into_inner(ordinary_ui));
        }
        // Any uncertainty retains private UI until this bounded diagnostic exits.
        let output = captured_stdout
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(err)
            .and_then(|result| result);
        let errors = captured_stderr
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(err)
            .and_then(|result| result);
        for (name, captured) in [("stdout", &output), ("stderr", &errors)] {
            match captured {
                Ok(bytes) => println!("ordinary broker {name}: {}", String::from_utf8_lossy(bytes)),
                Err(error) => println!("ordinary broker {name} capture failed: {error}"),
            }
        }
        result?;
        output?;
        errors?;
        Ok(())
    }

    fn admission_error(operation: &'static str, source: impl Into<io::Error>) -> io::Error {
        let source = source.into();
        io::Error::new(source.kind(), AdmissionError { operation, source })
    }

    fn launch_restricted_payload(
        root: &Path,
        write_sid: &str,
        desktop_path: &str,
    ) -> io::Result<()> {
        use firehazard::token;
        use firehazard_sid as firehazard;
        use std::os::windows::io::AsHandle as _;
        let host =
            firehazard::open_process_token(firehazard::get_current_process(), token::ALL_ACCESS)
                .map_err(|error| admission_error("OpenProcessToken(host)", error))?;
        let sid = firehazard::convert_string_sid_to_sid_w(write_sid)
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
        if restricting.group_count() != 1
            || !firehazard::equal_sid(restricting.groups()[0].sid, (&sid).into())
        {
            return Err(err(
                "filtered host must retain exactly the task restricting SID",
            ));
        }
        println!("filtered host: exactly one restricting SID, matching task");
        let old_default = filtered
            .default_dacl()
            .map_err(|error| admission_error("GetTokenInformation(default DACL)", error))?;
        let old_acl = old_default
            .default_dacl()
            .ok_or_else(|| err("proof requires a non-null default DACL"))?;
        let original_count = old_acl.aces().len();
        let mut default_acl = firehazard::acl::Builder::new(firehazard::acl::REVISION_DS);
        default_acl
            .add_acl(firehazard::acl::REVISION_DS, 0, old_acl)
            .map_err(|error| admission_error("AddAce(preserve default DACL)", error))?;
        default_acl
            .add_access_allowed_ace(
                firehazard::acl::REVISION_DS,
                firehazard::access::GENERIC_ALL,
                &sid,
            )
            .map_err(|error| admission_error("AddAccessAllowedAce(task default DACL)", error))?;
        default_acl.finish().map_err(err)?;
        filtered
            .set_default_dacl(&mut default_acl)
            .map_err(|error| admission_error("SetTokenInformation(task default DACL)", error))?;
        let readback = filtered.default_dacl().map_err(|error| {
            admission_error("GetTokenInformation(default DACL readback)", error)
        })?;
        let readback = readback
            .default_dacl()
            .ok_or_else(|| err("default DACL readback was null"))?;
        if readback.aces().len() != original_count + 1 {
            return Err(err("task default-DACL ACE readback count mismatch"));
        }
        // The old safe ACE wrapper exposes a header and Debug witness, but no
        // typed allowed-ACE SID accessor. Retain the actual appended ACE without
        // treating Debug formatting as a security predicate.
        println!(
            "filtered host default DACL: preserved {original_count} ACEs; appended {:?}",
            readback.aces().last()
        );
        let change_notify = firehazard::privilege::name::CHANGE_NOTIFY
            .lookup_luid()
            .map_err(err)?;
        let privileges = filtered.privileges().map_err(err)?;
        let notify_enabled = privileges.privileges().iter().any(|entry| {
            entry.luid == change_notify
                && entry.attributes & firehazard::privilege::ENABLED != Default::default()
        });
        println!(
            "filtered host: SeChangeNotifyPrivilege enabled={notify_enabled}; no privilege adjustments"
        );
        if !notify_enabled {
            return Err(err("filtered host lacks enabled SeChangeNotifyPrivilege"));
        }
        verify_ordinary_broker("immediately before CreateProcessAsUserW")?;
        launch_lpac_as(root, "filtered", Some(filtered.as_handle()), desktop_path)
    }
    #[cfg(test)]
    mod capture_tests {
        use super::*;

        struct TerminalError {
            bytes: io::Cursor<Vec<u8>>,
            kind: io::ErrorKind,
        }

        impl io::Read for TerminalError {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = self.bytes.read(buffer)?;
                if count != 0 {
                    return Ok(count);
                }
                Err(io::Error::from(self.kind))
            }
        }

        #[test]
        fn anonymous_pipe_closure_retains_actual_bytes() {
            #[cfg(windows)]
            assert_eq!(
                io::Error::from_raw_os_error(109).kind(),
                io::ErrorKind::BrokenPipe
            );
            let reader = TerminalError {
                bytes: io::Cursor::new(b"actual child witness".to_vec()),
                kind: io::ErrorKind::BrokenPipe,
            };
            assert_eq!(read_captured(reader).unwrap(), b"actual child witness");
        }

        #[test]
        fn other_read_errors_remain_errors() {
            let reader = TerminalError {
                bytes: io::Cursor::new(b"partial".to_vec()),
                kind: io::ErrorKind::PermissionDenied,
            };
            assert_eq!(
                read_captured(reader).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        #[test]
        fn capture_drains_beyond_retention_limit() {
            let mut reader = TerminalError {
                bytes: io::Cursor::new(vec![b'x'; 32 * 1024]),
                kind: io::ErrorKind::BrokenPipe,
            };
            assert_eq!(read_captured(&mut reader).unwrap(), vec![b'x'; 16 * 1024]);
            assert_eq!(reader.bytes.position(), 32 * 1024);
        }
    }
}
