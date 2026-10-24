//! Process-owned private UI for the synchronous, single-shot Windows helper.
//! Handles intentionally remain open on every post-creation return path. Only
//! helper exit releases them; the durable worker owns descendant quiescence.
use std::{io, mem::ManuallyDrop};

pub(super) struct ProcessDesktop {
    _desktop: ManuallyDrop<firehazard_sid::desktop::OwnedHandle>,
    pub(super) path: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{operation}: {source}")]
struct DesktopError {
    operation: &'static str,
    #[source]
    source: io::Error,
}
fn failure(operation: &'static str, source: impl Into<io::Error>) -> io::Error {
    io::Error::other(DesktopError {
        operation,
        source: source.into(),
    })
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

impl ProcessDesktop {
    pub(super) fn create(name: &str, user: &str, task: &str, package: &str) -> io::Result<Self> {
        use firehazard_sid as ui;
        let user = ui::convert_string_sid_to_sid_w(user).map_err(io::Error::from)?;
        let task = ui::convert_string_sid_to_sid_w(task).map_err(io::Error::from)?;
        let package = ui::convert_string_sid_to_sid_w(package).map_err(io::Error::from)?;
        let mut desktop_acl = ui::acl::Builder::new(ui::acl::REVISION);
        desktop_acl
            .add_access_allowed_ace(ui::acl::REVISION, ui::access::GENERIC_ALL, &user)
            .map_err(io::Error::from)?;
        // CreateProcessAsUser requires full access for the trusted helper.
        // Its task SID is granted this only on the fresh private desktop;
        // the AppContainer package keeps the narrower participant rights.
        desktop_acl
            .add_access_allowed_ace(ui::acl::REVISION, ui::access::GENERIC_ALL, &task)
            .map_err(io::Error::from)?;
        desktop_acl
            .add_access_allowed_ace(ui::acl::REVISION, desktop_participant(), &package)
            .map_err(io::Error::from)?;
        desktop_acl.finish().map_err(io::Error::from)?;
        let mut label = ui::acl::Builder::new(ui::acl::REVISION);
        // SYSTEM_MANDATORY_LABEL_NO_WRITE_UP, with a Low object label.
        label
            .add_mandatory_ace(
                ui::acl::REVISION,
                Default::default(),
                1,
                &ui::sid::integrity::Low,
            )
            .map_err(io::Error::from)?;
        label.finish().map_err(io::Error::from)?;
        let desktop_sd = ui::security::DescriptorBuilder::new()
            .dacl(desktop_acl.as_acl(), false)
            .map_err(io::Error::from)?
            .sacl(label.as_acl(), false)
            .map_err(io::Error::from)?
            .finish();
        // Create only the owned desktop in the helper's existing station.
        // Desktop-only lpDesktop retains that station without changing its ACL.
        let desktop = ui::create_desktop_w(
            name,
            (),
            None,
            None,
            ui::access::GENERIC_ALL,
            Some(&ui::security::Attributes::new(Some(&desktop_sd), false)),
        );
        let desktop = desktop.map_err(|error| failure("CreateDesktopW(private)", error))?;
        Ok(Self {
            _desktop: ManuallyDrop::new(desktop),
            path: name.to_owned(),
        })
    }
}
