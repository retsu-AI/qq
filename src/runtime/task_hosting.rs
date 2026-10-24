//! Host-only destination selection and credential resolution.
use super::*;
use qq_core::command_owner::{NativeDirectoryGrant, NativeWorkspaceIdentity};

const CREDENTIAL_ENDPOINT: &str = "https://github.com";

impl RuntimeFactory {
    pub(super) fn load_task_hosting(
        &self,
        workspace: NativeDirectoryGrant,
    ) -> qq_core::TaskHostingLoadFuture {
        let factory = self.clone();
        Box::pin(async move {
            let slots = factory
                .inner
                .check_policy_slots
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)));
            let permit = Arc::clone(slots)
                .acquire_owned()
                .await
                .map_err(|_| RuntimeLoadError {
                    kind: RunFailureKind::Server,
                    message: "hosting policy workers unavailable".into(),
                })?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                factory.task_hosting_blocking(&workspace)
            })
            .await
            .map_err(|_| RuntimeLoadError {
                kind: RunFailureKind::Server,
                message: "hosting policy preparation stopped unexpectedly".into(),
            })?
        })
    }

    fn task_hosting_blocking(
        &self,
        workspace: &NativeDirectoryGrant,
    ) -> Result<Option<qq_core::TaskHostingAuthorization>, RuntimeLoadError> {
        let failure = |message: String| RuntimeLoadError {
            kind: RunFailureKind::Configuration,
            message,
        };
        if workspace
            .path
            .canonicalize()
            .map_err(|error| failure(error.to_string()))?
            != workspace.path
            || NativeWorkspaceIdentity::capture(&workspace.path)
                .map_err(|error| failure(error.to_string()))?
                != workspace.identity
        {
            return Err(failure("task hosting workspace identity changed".into()));
        }
        let request = self
            .request_for_workspace(&workspace.path, None)
            .map_err(|error| failure(error.to_string()))?;
        let snapshot = self
            .load_for_client(&request)
            .map_err(|error| failure(error.to_string()))?;
        let mut selected = None;
        for grant in snapshot.policy().task_hosting() {
            let canonical = match grant.workspace().canonicalize() {
                Ok(path) => path,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(failure(error.to_string())),
            };
            let identity = NativeWorkspaceIdentity::capture(&canonical)
                .map_err(|error| failure(error.to_string()))?;
            if identity != workspace.identity {
                continue;
            }
            if selected.replace(grant).is_some() {
                return Err(failure(
                    "multiple hosting grants resolve to this workspace".into(),
                ));
            }
        }
        let Some(grant) = selected else {
            return Ok(None);
        };
        let credential = self
            .inner
            .credentials
            .resolve_with_endpoint(&grant.credential(), Some(CREDENTIAL_ENDPOINT))
            .map_err(|error| failure(error.to_string()))?;
        let token = credential
            .expose_secret_str()
            .map_err(|error| failure(error.to_string()))?;
        if token.is_empty()
            || token.len() > 16 * 1024
            || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(failure("GitHub credential is not a bounded token".into()));
        }
        Ok(Some(qq_core::TaskHostingAuthorization {
            owner: grant.owner().to_owned(),
            repository: grant.repository().to_owned(),
            base_branch: grant.base_branch().to_owned(),
            credential: qq_provider::SecretLiteral::new(token),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tests::{MemoryKeyring, RuntimeFixture};
    use qq_auth::CredentialPaths;

    #[tokio::test]
    async fn hosting_authority_matches_current_workspace_and_revocation() {
        let fixture = RuntimeFixture::new();
        let store = CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        );
        store
            .set_with_metadata(
                "github/work",
                b"host-only-fixture-token",
                false,
                None,
                Some(CREDENTIAL_ENDPOINT),
            )
            .unwrap();
        let factory = fixture.factory_with_credentials(store);
        let workspace = NativeDirectoryGrant::capture(&fixture.path("work")).unwrap();
        assert!(
            factory
                .authorize_task_hosting(workspace.clone())
                .await
                .unwrap()
                .is_none()
        );
        let config = fixture.path("global/config.ron");
        let grant = |path: &Path| {
            format!(
                r#"(workspace:{}, owner:"retsu-AI", repository:"qq", base_branch:"main", credential:Stored("github/work"))"#,
                serde_json::to_string(path).unwrap()
            )
        };
        std::fs::write(
            &config,
            format!(
                "(version:1, policy:(task_hosting:[{}]))",
                grant(&workspace.path)
            ),
        )
        .unwrap();
        let authorized = factory
            .authorize_task_hosting(workspace.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(authorized.owner, "retsu-AI");
        assert_eq!(authorized.repository, "qq");
        assert_eq!(authorized.base_branch, "main");
        assert_eq!(
            authorized.credential.expose_secret(),
            "host-only-fixture-token"
        );
        assert!(!format!("{authorized:?}").contains("host-only-fixture-token"));
        std::fs::create_dir(fixture.path("other")).unwrap();
        assert!(
            factory
                .authorize_task_hosting(
                    NativeDirectoryGrant::capture(&fixture.path("other")).unwrap()
                )
                .await
                .unwrap()
                .is_none()
        );
        std::fs::write(&config, "(version:1, policy:(task_hosting:[]))").unwrap();
        assert!(
            factory
                .authorize_task_hosting(workspace.clone())
                .await
                .unwrap()
                .is_none()
        );
        #[cfg(unix)]
        {
            let alias = fixture.path("alias");
            std::os::unix::fs::symlink(&workspace.path, &alias).unwrap();
            std::fs::write(
                &config,
                format!(
                    "(version:1, policy:(task_hosting:[{},{}]))",
                    grant(&workspace.path),
                    grant(&alias)
                ),
            )
            .unwrap();
            let error = factory.authorize_task_hosting(workspace).await.unwrap_err();
            assert!(error.message.contains("multiple hosting grants"), "{error}");
        }
    }
}
