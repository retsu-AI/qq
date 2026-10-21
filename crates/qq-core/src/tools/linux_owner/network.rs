//! Private IP networking plus denial of host pathname Unix sockets.
use landlock::{AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetStatus, Scope};
use std::io;

pub(super) fn prepare() -> io::Result<()> {
    // A private network namespace does not isolate pathname Unix sockets.
    // Probe the required LSM rights without restricting the worker.
    drop(ruleset()?);
    Ok(())
}

pub(super) fn enforce() -> io::Result<()> {
    let status = ruleset()?.restrict_self().map_err(io::Error::other)?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "required Linux network enforcement is unavailable",
        ));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("required Linux network enforcement is unavailable: {0}")]
struct Unavailable(#[source] landlock::RulesetError);

fn ruleset() -> io::Result<landlock::RulesetCreated> {
    let create = || {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::ResolveUnix)?
            .scope(Scope::AbstractUnixSocket | Scope::Signal)?
            .create()
    };
    create().map_err(|error| io::Error::new(io::ErrorKind::Unsupported, Unavailable(error)))
}
