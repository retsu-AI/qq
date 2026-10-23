//! Windows' process-security environment; no tool-tree ACL mutation.
use super::{Packet, READY, access, arguments, directory, resources, unix_ms};
use mxc_sdk::v1::{
    self, ClipboardPolicy, ContainerRequest, Containment, FilesystemPolicy, NetworkAction,
    NetworkEgressPolicy, NetworkIngressPolicy, NetworkPolicy, UiPolicy,
    configs::{ProcessContainerConfig, ProcessContainerFilesystem},
};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs::File,
    io::{self, Read as _, Write as _},
};

fn text(value: &OsStr) -> io::Result<String> {
    value.to_str().map(str::to_owned).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "native Windows policy requires Unicode paths and environment values",
        )
    })
}

fn request(packet: &Packet) -> io::Result<ContainerRequest> {
    let crate::command_owner::NativeFilesystem::Restricted {
        read_only,
        read_write,
    } = &packet.filesystem
    else {
        return Err(io::Error::other(
            "native Windows policy requires explicit file authority",
        ));
    };
    let mut ancestors = BTreeSet::new();
    for root in read_only.iter().chain(read_write) {
        for ancestor in root.path.ancestors().skip(1) {
            ancestors.insert(text(ancestor.as_os_str())?);
        }
    }
    let mut filesystem = ProcessContainerFilesystem::default();
    filesystem.enumerate_paths = ancestors.into_iter().collect();
    let mut config = ProcessContainerConfig::default();
    config.capabilities = vec!["registryRead".to_owned()];
    config.filesystem = Some(filesystem);
    let mut request =
        ContainerRequest::new(arguments::command_line(&packet.program, &packet.arguments)?);
    request.containment = Containment::ProcessContainer(config);
    request.working_directory = Some(text(
        directory::working_directory(&packet.program, &packet.cwd)?.as_os_str(),
    )?);
    request.environment = Some(
        packet
            .environment
            .iter()
            .map(|(name, value)| Ok((text(name)?, text(value)?)))
            .collect::<io::Result<_>>()?,
    );
    request.inherit_default_environment = Some(false);
    request.filesystem = Some(FilesystemPolicy {
        readonly_paths: read_only
            .iter()
            .map(|root| text(root.path.as_os_str()))
            .collect::<io::Result<_>>()?,
        readwrite_paths: read_write
            .iter()
            .map(|root| text(root.path.as_os_str()))
            .collect::<io::Result<_>>()?,
        ..Default::default()
    });
    request.network = Some(NetworkPolicy {
        egress: Some(NetworkEgressPolicy {
            default: Some(NetworkAction::Deny),
            ..Default::default()
        }),
        ingress: Some(NetworkIngressPolicy {
            default: Some(NetworkAction::Deny),
            host_loopback: Some(NetworkAction::Deny),
        }),
        ..Default::default()
    });
    // Console runtimes initialize User32. Clipboard and input injection remain denied.
    request.ui = Some(UiPolicy {
        disable: false,
        clipboard: ClipboardPolicy::None,
        allow_input_injection: false,
    });
    // V1 maps None to timeout=0 (unbounded). The QQ owner enforces the persisted
    // deadline and terminates the entire enclosing job, including this helper.
    request.timeout_ms = None;
    Ok(request)
}

pub(super) fn available() -> bool {
    mxc_sdk::process_container_common::ProcessSecurityEnvironmentApi::load()
        .and_then(|api| api.supports_version(1, 1))
        .unwrap_or(false)
}

pub(super) fn execute(packet: Packet, mut input: File) -> io::Result<u8> {
    let _roots = access::hold(&packet.filesystem, packet.not_after_ms)?;
    let request = request(&packet)?;
    let probe = v1::probe(Some(&request)).map_err(io::Error::other)?;
    if probe.tier != Some("base-container")
        || probe.needs_dacl_augmentation != Some(false)
        || !probe.warnings.is_empty()
        || probe.error.is_some()
        || !probe.probes.base_container_supports_enumerate_paths
    {
        return Err(io::Error::other(
            "native Windows policy cannot enforce the requested scope without host ACL changes",
        ));
    }
    let cpu = resources::apply(packet.resources)?;
    let _cpu = resources::apply_native_cpu(cpu)?;
    std::io::stderr().write_all(READY)?;
    let mut release = [0];
    input.read_exact(&mut release)?;
    if release != [1] || unix_ms() >= packet.not_after_ms {
        return Err(io::Error::other("native authorization expired"));
    }
    // No payload exists until authorization. Creation installs the native file,
    // network and UI policy atomically; failure never selects a host fallback.
    let mut child = v1::spawn(request, Default::default()).map_err(io::Error::other)?;
    if !child.warnings().is_empty() {
        child.kill()?;
        let _ = child.wait();
        return Err(io::Error::other(
            "native Windows launch reported degraded enforcement",
        ));
    }
    let mut stdin = child
        .take_stdin()
        .ok_or_else(|| io::Error::other("native stdin is missing"))?;
    let mut stdout = child
        .take_stdout()
        .ok_or_else(|| io::Error::other("native stdout is missing"))?;
    let mut stderr = child
        .take_stderr()
        .ok_or_else(|| io::Error::other("native stderr is missing"))?;
    // The helper exits the process after the output is drained. Its stdin pump
    // may still be blocked on the worker's pipe; it must not delay settlement.
    drop(
        std::thread::Builder::new()
            .name("qq-native-stdin".into())
            .spawn(move || io::copy(&mut input, &mut stdin))?,
    );
    let out = std::thread::Builder::new()
        .name("qq-native-stdout".into())
        .spawn(move || io::copy(&mut stdout, &mut std::io::stdout().lock()))?;
    let err = std::thread::Builder::new()
        .name("qq-native-stderr".into())
        .spawn(move || io::copy(&mut stderr, &mut std::io::stderr().lock()))?;
    let outcome = child.wait()?;
    out.join()
        .map_err(|_| io::Error::other("native stdout pump failed"))??;
    err.join()
        .map_err(|_| io::Error::other("native stderr pump failed"))??;
    drop(child);
    let code = match outcome {
        v1::WaitResult::Exited(code) => code,
        v1::WaitResult::TimedOut => {
            return Err(io::Error::other(
                "native Windows policy unexpectedly timed out",
            ));
        }
    };
    std::process::exit(code)
}
