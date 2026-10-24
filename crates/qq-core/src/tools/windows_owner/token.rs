//! A write-only restricting SID composes with AppContainer's read and network boundary.
use firehazard_sid as fh;
use std::io;

#[derive(Debug, thiserror::Error)]
#[error("{operation}: {source}")]
struct TokenError {
    operation: &'static str,
    #[source]
    source: io::Error,
}
fn at<T>(operation: &'static str, result: Result<T, fh::Error>) -> io::Result<T> {
    result.map_err(|source| {
        io::Error::other(TokenError {
            operation,
            source: source.into(),
        })
    })
}

pub(super) fn create(task: &str) -> io::Result<fh::token::OwnedHandle> {
    let host = at(
        "open helper primary token",
        fh::open_process_token(fh::get_current_process(), fh::token::ALL_ACCESS),
    )?;
    let sid = at(
        "parse task write SID",
        fh::convert_string_sid_to_sid_w(task),
    )?;
    let restricting = [fh::sid::AndAttributes::new(&sid, None)];
    let token = at(
        "restrict task writes",
        fh::create_restricted_token(
            &host,
            fh::token::DISABLE_MAX_PRIVILEGE | fh::token::WRITE_RESTRICTED,
            None,
            None,
            Some(&restricting),
        ),
    )?;
    let actual = at("query task restricting SID", token.restricted_sids())?;
    if actual.group_count() != 1 || !fh::equal_sid(actual.groups()[0].sid, (&sid).into()) {
        return Err(io::Error::other(
            "task token must have exactly its own write restricting SID",
        ));
    }
    // New process/thread/IPC objects also need the restricting SID for writes.
    // Preserve the caller's default ACL; never add broad SID entries as restrictors.
    let old = at("read token default DACL", token.default_dacl())?;
    let old_acl = old
        .default_dacl()
        .ok_or_else(|| io::Error::other("task token default DACL is null"))?;
    let mut acl = fh::acl::Builder::new(fh::acl::REVISION_DS);
    at(
        "preserve token default DACL",
        acl.add_acl(fh::acl::REVISION_DS, 0, old_acl),
    )?;
    at(
        "grant task object writes",
        acl.add_access_allowed_ace(fh::acl::REVISION_DS, fh::access::GENERIC_ALL, &sid),
    )?;
    at("finish token default DACL", acl.finish())?;
    at(
        "install token default DACL",
        token.set_default_dacl(&mut acl),
    )?;
    let notify = at(
        "resolve traversal privilege",
        fh::privilege::name::CHANGE_NOTIFY.lookup_luid(),
    )?;
    let privileges = at("query task privileges", token.privileges())?;
    if !privileges.privileges().iter().any(|entry| {
        entry.luid == notify && entry.attributes & fh::privilege::ENABLED != Default::default()
    }) {
        return Err(io::Error::other(
            "task token requires the caller's enabled traversal privilege",
        ));
    }
    Ok(token)
}
