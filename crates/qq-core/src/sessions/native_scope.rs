//! Store-owned workspace authority, independent of model/profile reloads.
use super::{OptionalExtension, SessionRuntimeError, Store, store::Priority};
use crate::command_owner::NativeCommandScope;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_SCOPES: usize = 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const KEY: &str = "native_command_scopes";

#[derive(Debug)]
pub(crate) struct NativeScopes(BTreeMap<PathBuf, Arc<NativeCommandScope>>);

impl NativeScopes {
    pub(crate) fn single(scope: NativeCommandScope) -> Result<Arc<Self>, SessionRuntimeError> {
        let mut scopes = BTreeMap::new();
        insert(&mut scopes, scope)?;
        Ok(Arc::new(Self(scopes)))
    }

    /// Reject an unregistered or replaced root before project code is loaded.
    pub(crate) async fn admit(&self, path: &Path) -> Result<(), String> {
        let scope = self.get(path)?;
        let path = path.to_owned();
        let permit = crate::workspace::blocking_permits()
            .acquire_owned()
            .await
            .map_err(|error| error.to_string())?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let identity = crate::command_owner::NativeWorkspaceIdentity::capture(&path)?;
            scope.validate_workspace(&path, identity)
        })
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
    }

    pub(crate) async fn validate_opened(
        &self,
        workspace: crate::workspace::Workspace,
    ) -> Result<(), String> {
        let scope = self.get(workspace.path())?;
        let permit = crate::workspace::blocking_permits()
            .acquire_owned()
            .await
            .map_err(|error| error.to_string())?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let identity = crate::command_owner::NativeWorkspaceIdentity::held(workspace.root())?;
            scope.validate_workspace(workspace.path(), identity)
        })
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
    }

    pub(crate) fn get(&self, path: &Path) -> Result<Arc<NativeCommandScope>, String> {
        self.0
            .get(path)
            .cloned()
            .ok_or_else(|| "workspace has no registered native execution scope".to_owned())
    }
}

/// Registration is additive. Omitting registrations restores stored authority;
/// it never disables it. Existing task authority cannot be replaced implicitly.
pub(super) async fn load(
    store: &Store,
    added: Vec<NativeCommandScope>,
) -> Result<Option<Arc<NativeScopes>>, SessionRuntimeError> {
    if added.len() > MAX_SCOPES {
        return Err(invalid("native scope capacity exhausted"));
    }
    store.call_write(Priority::AwaitControl, move |connection| {
        let transaction = super::store::begin_unit(connection)?;
        let saved: Option<Option<String>> = transaction.query_row(
            "SELECT CASE WHEN length(CAST(value AS BLOB))<=?2 THEN value ELSE NULL END FROM metadata WHERE key=?1",
            rusqlite::params![KEY, MAX_BYTES], |row| row.get(0),
        ).optional()?;
        let mut policies = BTreeMap::new();
        if let Some(saved) = saved {
            let saved = saved.ok_or_else(|| invalid("stored scope policy exceeds its byte limit"))?;
            let scopes: Vec<NativeCommandScope> = serde_json::from_str(&saved)?;
            for scope in scopes { insert(&mut policies, scope)?; }
            if policies.is_empty() { return Err(invalid("stored scope policy is empty")); }
        }
        let changed = !added.is_empty();
        for scope in added { insert(&mut policies, scope)?; }
        if policies.is_empty() { return Ok(None); }
        if changed {
            let mut encoded = String::from("[");
            for (index, scope) in policies.values().enumerate() {
                let item = serde_json::to_string(scope)?;
                if encoded.len() + item.len() + 2 > MAX_BYTES { return Err(invalid("scope policy exceeds its byte limit")); }
                if index != 0 { encoded.push(','); }
                encoded.push_str(&item);
            }
            encoded.push(']');
            transaction.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", rusqlite::params![KEY, encoded])?;
        }
        transaction.commit()?;
        Ok(Some(Arc::new(NativeScopes(policies))))
    }).await
}

fn insert(
    policies: &mut BTreeMap<PathBuf, Arc<NativeCommandScope>>,
    mut scope: NativeCommandScope,
) -> Result<(), SessionRuntimeError> {
    scope
        .resources
        .validate()
        .map_err(|error| invalid(&error.to_string()))?;
    if scope.executable_directories.len() > 16
        || scope
            .executable_directories
            .iter()
            .any(|path| !scope.read_only.iter().any(|grant| grant.path == *path))
    {
        return Err(invalid(
            "executable search roots must be admitted read-only",
        ));
    }
    if scope.read_only.len() > 63
        || scope
            .read_only
            .iter()
            .chain(std::iter::once(&scope.workspace))
            .any(|grant| {
                !grant.path.is_absolute()
                    || grant.path.as_os_str().len() > 4096
                    || grant.path.components().any(|part| {
                        matches!(
                            part,
                            std::path::Component::ParentDir | std::path::Component::CurDir
                        )
                    })
            })
    {
        return Err(invalid(
            "scope roots must be bounded canonical absolute paths",
        ));
    }
    if let Some(sdk) = &scope.sdk_environment {
        sdk.validate(&scope)
            .map_err(|error| invalid(&error.to_string()))?;
    }
    scope.read_only.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(old) = policies.get(&scope.workspace.path) {
        if serde_json::to_vec(old)? != serde_json::to_vec(&scope)? {
            return Err(invalid(
                "task workspace scope conflicts with persisted authority",
            ));
        }
    } else {
        if policies.len() >= MAX_SCOPES {
            return Err(invalid("native scope capacity exhausted"));
        }
        policies.insert(scope.workspace.path.clone(), Arc::new(scope));
    }
    Ok(())
}

fn invalid(reason: &str) -> SessionRuntimeError {
    SessionRuntimeError::InvalidNativeScope {
        reason: reason.to_owned(),
    }
}
