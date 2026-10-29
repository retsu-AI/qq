use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use qq_protocol::CredentialEpoch;
use qq_provider::{
    RequestCredential, RequestCredentialError, RequestCredentialFuture, RequestCredentialProvider,
};

use super::{AuthError, CredentialStore, oauth};

pub(crate) trait RequestCredentialSource: Send + Sync + 'static {
    type Credential: Clone + Send + Sync + 'static;

    fn resolve(
        &self,
        store: &CredentialStore,
        reload: bool,
    ) -> Result<Resolution<Self::Credential>, AuthError>;

    fn request_credential(
        credential: &Self::Credential,
    ) -> Result<RequestCredential, RequestCredentialError>;

    fn map_error(&self, error: AuthError) -> RequestCredentialError;

    fn clock_unavailable() -> AuthError;

    fn never_cacheable(&self) -> bool {
        false
    }
}

pub(crate) struct Resolution<C> {
    pub(crate) credential: C,
    pub(crate) reuse: Option<Reuse>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reuse {
    pub(crate) epoch: CredentialEpoch,
    pub(crate) refresh_after: u64,
}

pub(crate) struct Cached<C> {
    pub(crate) credential: C,
    pub(crate) epoch: CredentialEpoch,
    pub(crate) refresh_after: u64,
}

impl<C> Cached<C> {
    fn reuse(&self) -> Reuse {
        Reuse {
            epoch: self.epoch,
            refresh_after: self.refresh_after,
        }
    }
}

pub(crate) struct CachedRequestCredentials<S: RequestCredentialSource> {
    pub(crate) store: CredentialStore,
    source: Arc<S>,
    pub(crate) cache: tokio::sync::Mutex<Option<Arc<Cached<S::Credential>>>>,
    pub(crate) fill: tokio::sync::Mutex<()>,
    uncacheable: AtomicBool,
    #[cfg(test)]
    pub(crate) fill_attempts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    resolved_under_fill: std::sync::atomic::AtomicUsize,
}

impl<S: RequestCredentialSource> CachedRequestCredentials<S> {
    pub(crate) fn new(store: &CredentialStore, source: S) -> Self {
        let never_cacheable = source.never_cacheable();
        Self {
            store: store.clone(),
            source: Arc::new(source),
            cache: tokio::sync::Mutex::new(None),
            fill: tokio::sync::Mutex::new(()),
            uncacheable: AtomicBool::new(never_cacheable),
            #[cfg(test)]
            fill_attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            resolved_under_fill: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn resolved_under_fill_for_test(&self) -> usize {
        self.resolved_under_fill.load(Ordering::SeqCst)
    }

    async fn still_valid(&self, reuse: Reuse) -> Result<bool, RequestCredentialError> {
        self.store
            .load_request_credential(move |store| {
                let now = oauth::unix_time().map_err(|_| S::clock_unavailable())?;
                Ok(store.epoch()? == reuse.epoch && now < reuse.refresh_after)
            })
            .await?
            .map_err(|error| self.source.map_error(error))
    }

    async fn resolve_uncached(&self) -> Result<RequestCredential, RequestCredentialError> {
        let source = Arc::clone(&self.source);
        let resolution = self
            .store
            .load_request_credential(move |store| source.resolve(&store, false))
            .await?
            .map_err(|error| self.source.map_error(error))?;
        self.store_resolution(resolution).await
    }

    async fn store_resolution(
        &self,
        resolution: Resolution<S::Credential>,
    ) -> Result<RequestCredential, RequestCredentialError> {
        let request_credential = S::request_credential(&resolution.credential)?;
        match resolution.reuse {
            Some(reuse) => {
                self.uncacheable.store(false, Ordering::Relaxed);
                *self.cache.lock().await = Some(Arc::new(Cached {
                    credential: resolution.credential,
                    epoch: reuse.epoch,
                    refresh_after: reuse.refresh_after,
                }));
            }
            None => {
                self.uncacheable.store(true, Ordering::Relaxed);
                *self.cache.lock().await = None;
            }
        }
        Ok(request_credential)
    }
}

impl<S: RequestCredentialSource> RequestCredentialProvider for CachedRequestCredentials<S> {
    fn credential(&self) -> RequestCredentialFuture<'_> {
        Box::pin(async move {
            let snapshot = self.cache.lock().await.clone();
            if let Some(cached) = &snapshot {
                if self.still_valid(cached.reuse()).await? {
                    return S::request_credential(&cached.credential);
                }
            } else if self.uncacheable.load(Ordering::Relaxed) {
                return self.resolve_uncached().await;
            }

            #[cfg(test)]
            self.fill_attempts.fetch_add(1, Ordering::SeqCst);
            let fill = self.fill.lock().await;
            if self.uncacheable.load(Ordering::Relaxed) {
                drop(fill);
                return self.resolve_uncached().await;
            }
            let current = self.cache.lock().await.clone();
            let current_reuse = current.as_deref().map(Cached::reuse);
            #[cfg(test)]
            self.resolved_under_fill.fetch_add(1, Ordering::SeqCst);
            let source = Arc::clone(&self.source);
            let resolved = self
                .store
                .load_request_credential(move |store| {
                    if let Some(reuse) = current_reuse {
                        let now = oauth::unix_time().map_err(|_| S::clock_unavailable())?;
                        if store.epoch()? == reuse.epoch && now < reuse.refresh_after {
                            return Ok(None);
                        }
                    }
                    source.resolve(&store, current_reuse.is_some()).map(Some)
                })
                .await;
            let failed = |error| {
                self.uncacheable.store(true, Ordering::Relaxed);
                error
            };
            let resolved = match resolved {
                Ok(Ok(resolved)) => resolved,
                Ok(Err(error)) => {
                    drop(fill);
                    return Err(failed(self.source.map_error(error)));
                }
                Err(error) => {
                    drop(fill);
                    return Err(failed(error));
                }
            };
            match (resolved, current) {
                (Some(resolution), _) => {
                    let credential = self.store_resolution(resolution).await;
                    drop(fill);
                    credential
                }
                (None, Some(cached)) => S::request_credential(&cached.credential),
                (None, None) => unreachable!("a cache hit requires a cached credential"),
            }
        })
    }
}
