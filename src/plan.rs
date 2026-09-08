//! Bounded cache of compiled agent plans with filesystem revalidation.
//!
//! Every durable run used to reload configuration, resolve credentials, and
//! reopen the workspace. The cache keeps one live [`CompiledAgentPlan`]
//! generation per (workspace, model selection, explicit config) key and
//! revalidates it with a fixed list of `stat` calls — the paths the config
//! loader probed, the credential index, the workspace instruction files, and
//! the skill roots — plus one synchronous generation check per external tool
//! host. Any observable change recompiles and atomically swaps in a new
//! generation for later runs; runs already holding the old `Arc` keep it.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use qq_config::{AwsAuth, BedrockAuth, HttpCredential, ProviderAccess, ProviderAuth};
use qq_core::plan::{CompiledAgentPlan, SourceFingerprint};
use qq_protocol::{AgentPlanDigest, AgentProfileId, CredentialEpoch, ModelSelection};
use qq_provider::SecretRef;
use thiserror::Error;

/// Hard admission bounds. Active generations count toward `max_bytes` and are
/// never evicted; when inactive eviction cannot make room, admission fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCacheLimits {
    pub max_entries: usize,
    pub max_bytes: usize,
}

impl Default for PlanCacheLimits {
    /// Sixteen generations and 64 MiB of estimated plan heap: enough for a
    /// TUI switching among a handful of models and workspaces, small enough
    /// that a runaway refresh cannot grow the process without bound.
    fn default() -> Self {
        Self {
            max_entries: 16,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Identity of one cache slot. Two requests that would load configuration
/// identically share a slot; anything that changes the load request itself is
/// part of the key rather than a revalidated source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlanKey {
    pub workspace: PathBuf,
    pub model: ModelSelection,
    pub profile: AgentProfileId,
    pub explicit_config_path: Option<PathBuf>,
    pub explicit_config_content: Option<String>,
}

/// Everything a compile produced that the cache needs to keep beside the plan.
pub struct CompiledGeneration {
    pub plan: Arc<CompiledAgentPlan>,
    pub configuration_sources: Vec<qq_config::ConfigSources>,
    /// Paths whose state decided this compile; re-stat'd on every lookup.
    pub sources: Vec<SourceFingerprint>,
    pub bindings: LiveBindings,
}

/// Equality of live configuration is distinct from durable plan identity.
/// This root-only payload is neither hashed nor serialized; full endpoints,
/// inline credentials, and header values must never enter diagnostics.
#[derive(Default)]
pub struct LiveBindings {
    pub provider: Option<ProviderAccess>,
    pub mcp: Option<Arc<crate::mcp::WiredMcpRegistry>>,
}

impl PartialEq for LiveBindings {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && match (&self.mcp, &other.mcp) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for LiveBindings {}

impl std::fmt::Debug for LiveBindings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LiveBindings(<redacted>)")
    }
}

impl CompiledGeneration {
    fn estimated_bytes(&self) -> usize {
        let secret_bytes = |secret: &SecretRef| match secret {
            SecretRef::Env(name) | SecretRef::Stored(name) => name.len(),
            SecretRef::Value(value) => value.expose_secret().len(),
        };
        let bedrock_bytes = |region: &Option<String>, auth: &BedrockAuth| {
            region.as_ref().map_or(0, String::len)
                + match auth {
                    BedrockAuth::Aws(AwsAuth::DefaultChain) => 0,
                    BedrockAuth::Aws(AwsAuth::Profile(profile)) => profile.len(),
                    BedrockAuth::ApiKey(secret) => secret_bytes(secret),
                }
        };
        let binding_heap = match &self.bindings.provider {
            None => 0,
            Some(ProviderAccess::Http(access)) => {
                let auth = match access.auth() {
                    HttpCredential::Configured(auth) => match auth {
                        ProviderAuth::NoAuth => 0,
                        ProviderAuth::ApiKey(secret) | ProviderAuth::Bearer(secret) => {
                            secret_bytes(secret)
                        }
                        ProviderAuth::Header(name, secret) => name.len() + secret_bytes(secret),
                    },
                    HttpCredential::ApiKey { explicit, .. } => {
                        explicit.as_ref().map_or(0, secret_bytes)
                    }
                    HttpCredential::OpenAiCodex { profile } => {
                        profile.as_ref().map_or(0, String::len)
                    }
                    HttpCredential::XAi { api_key, profile } => {
                        api_key.as_ref().map_or(0, secret_bytes)
                            + profile.as_ref().map_or(0, String::len)
                    }
                };
                access.endpoint().len()
                    + auth
                    + access
                        .headers()
                        .iter()
                        .map(|(name, value)| {
                            // Estimated B-tree node overhead plus each owned string.
                            std::mem::size_of::<(String, qq_config::StaticHeaderValue)>()
                                + 3 * std::mem::size_of::<usize>()
                                + name.len()
                                + value.expose_value().len()
                        })
                        .sum::<usize>()
            }
            Some(
                ProviderAccess::AmazonBedrock { region, auth }
                | ProviderAccess::AmazonBedrockMantle { region, auth, .. },
            ) => bedrock_bytes(region, auth),
        };
        self.plan
            .estimated_bytes()
            .saturating_add(std::mem::size_of::<LiveBindings>())
            .saturating_add(binding_heap)
            .saturating_add(
                self.configuration_sources.capacity()
                    * std::mem::size_of::<qq_config::ConfigSources>(),
            )
            .saturating_add(
                self.configuration_sources
                    .iter()
                    .map(qq_config::ConfigSources::estimated_bytes)
                    .sum::<usize>(),
            )
    }
}

#[derive(Debug, Error)]
pub enum PlanCacheError<E> {
    #[error(transparent)]
    Compile(E),
    #[error(
        "plan cache is full: {active_entries} active generations hold {active_bytes} bytes of the \
         {max_bytes}-byte limit and the new plan needs {requested_bytes}"
    )]
    Capacity {
        active_entries: usize,
        active_bytes: usize,
        max_bytes: usize,
        requested_bytes: usize,
    },
    #[error("plan cache has been shut down")]
    ShutDown,
    #[error("plan cache lock was poisoned")]
    Poisoned,
}

/// Why a lookup produced the plan it did; surfaced for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLookup {
    /// Every recorded source fingerprint still matched.
    Hit,
    /// A source changed, but the recompiled plan had the same digest and
    /// epoch and live bindings; the existing generation was kept and its
    /// fingerprints refreshed.
    Revalidated,
    /// A new generation was compiled and published.
    Compiled,
}

struct Slot {
    key: PlanKey,
    generation: CompiledGeneration,
    digest: AgentPlanDigest,
    epoch: CredentialEpoch,
}

struct State {
    /// Most recently used at the back.
    slots: VecDeque<Slot>,
    shut_down: bool,
}

/// Bounded, revalidating cache of compiled plans. Cheap to clone; all clones
/// share one state.
#[derive(Clone)]
pub struct PlanCache {
    inner: Arc<Inner>,
}

struct Inner {
    limits: PlanCacheLimits,
    state: Mutex<State>,
    /// One compile per key at a time. A refresh storm on one workspace
    /// compiles once; other keys proceed independently.
    in_flight: Mutex<HashMap<PlanKey, Arc<Mutex<()>>>>,
}

impl PlanCache {
    #[must_use]
    pub fn new(limits: PlanCacheLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                limits,
                state: Mutex::new(State {
                    slots: VecDeque::new(),
                    shut_down: false,
                }),
                in_flight: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Returns the current generation for `key`, compiling one when none is
    /// cached or a recorded source changed. `compile` runs on the calling
    /// thread while this key's single-flight lock is held, so callers invoke
    /// this from a blocking context. Returns the plan and how it was obtained.
    pub fn load<E, F>(
        &self,
        key: PlanKey,
        compile: F,
    ) -> Result<(Arc<CompiledAgentPlan>, PlanLookup), PlanCacheError<E>>
    where
        F: FnOnce() -> Result<CompiledGeneration, E>,
    {
        let flight = {
            let mut in_flight = self
                .inner
                .in_flight
                .lock()
                .map_err(|_| PlanCacheError::Poisoned)?;
            Arc::clone(in_flight.entry(key.clone()).or_default())
        };
        let _flight = flight.lock().map_err(|_| PlanCacheError::Poisoned)?;

        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| PlanCacheError::Poisoned)?;
            if state.shut_down {
                return Err(PlanCacheError::ShutDown);
            }
            if let Some(index) = state.slots.iter().position(|slot| slot.key == key) {
                let slot = &state.slots[index];
                let current = slot
                    .generation
                    .sources
                    .iter()
                    .all(SourceFingerprint::is_current)
                    && slot
                        .generation
                        .configuration_sources
                        .iter()
                        .all(qq_config::ConfigSources::is_current)
                    && slot.generation.plan.hosts_are_current();
                if current {
                    let slot = state
                        .slots
                        .remove(index)
                        .expect("a located slot must exist");
                    let plan = Arc::clone(&slot.generation.plan);
                    state.slots.push_back(slot);
                    return Ok((plan, PlanLookup::Hit));
                }
            }
        }

        // Compile outside the state lock: other keys keep hitting meanwhile.
        let generation = compile().map_err(PlanCacheError::Compile)?;
        let digest = generation.plan.digest();
        let epoch = generation.plan.credential_epoch();

        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| PlanCacheError::Poisoned)?;
        if state.shut_down {
            return Err(PlanCacheError::ShutDown);
        }
        if let Some(index) = state.slots.iter().position(|slot| slot.key == key) {
            let mut slot = state
                .slots
                .remove(index)
                .expect("a located slot must exist");
            if slot.digest == digest
                && slot.epoch == epoch
                && slot.generation.bindings == generation.bindings
            {
                // Same behavior, same credentials: keep the live generation
                // that active runs may hold and only refresh what we watch.
                slot.generation.sources = generation.sources;
                slot.generation.configuration_sources = generation.configuration_sources;
                let plan = Arc::clone(&slot.generation.plan);
                state.slots.push_back(slot);
                return Ok((plan, PlanLookup::Revalidated));
            }
            // A changed generation is dropped from the cache; active runs
            // keep their own `Arc` until they settle.
        }

        let requested_bytes = generation.estimated_bytes();
        admit(&mut state, self.inner.limits, requested_bytes)?;
        let plan = Arc::clone(&generation.plan);
        state.slots.push_back(Slot {
            key,
            generation,
            digest,
            epoch,
        });
        Ok((plan, PlanLookup::Compiled))
    }

    /// Drops every cached generation and refuses further loads. Active runs
    /// keep the plans they hold.
    pub fn shutdown(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.shut_down = true;
            state.slots.clear();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.state.lock().map_or(0, |state| state.slots.len())
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Estimated bytes held by cached generations.
    #[cfg(test)]
    fn estimated_bytes(&self) -> usize {
        self.inner.state.lock().map_or(0, |state| {
            state
                .slots
                .iter()
                .map(|slot| slot.generation.estimated_bytes())
                .sum()
        })
    }
}

/// Makes room for `requested_bytes` by evicting least-recently-used inactive
/// generations. A generation is active while anything outside the cache holds
/// its `Arc`; those are pinned and count toward the limit.
fn admit<E>(
    state: &mut State,
    limits: PlanCacheLimits,
    requested_bytes: usize,
) -> Result<(), PlanCacheError<E>> {
    loop {
        let used_bytes: usize = state
            .slots
            .iter()
            .map(|slot| slot.generation.estimated_bytes())
            .sum();
        let fits = state.slots.len() < limits.max_entries
            && used_bytes.saturating_add(requested_bytes) <= limits.max_bytes;
        if fits {
            return Ok(());
        }
        let evictable = state
            .slots
            .iter()
            .position(|slot| Arc::strong_count(&slot.generation.plan) == 1);
        match evictable {
            Some(index) => {
                state.slots.remove(index);
            }
            None => {
                return Err(PlanCacheError::Capacity {
                    active_entries: state.slots.len(),
                    active_bytes: used_bytes,
                    max_bytes: limits.max_bytes,
                    requested_bytes,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use futures_util::stream;
    use qq_core::{Runtime, plan::AgentProfile};
    use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};

    use super::*;

    struct SilentProvider;

    impl Provider for SilentProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
        }
    }

    fn key(workspace: &Path, model: &str) -> PlanKey {
        PlanKey {
            workspace: workspace.to_owned(),
            model: ModelSelection {
                model: Some(model.to_owned()),
                max_output_tokens: None,
                organization: None,
            },
            profile: AgentProfileId::default(),
            explicit_config_path: None,
            explicit_config_content: None,
        }
    }

    /// Compiles an embedded plan for `workspace`; `model` varies the digest.
    fn compile(workspace: &Path, model: &str) -> CompiledGeneration {
        let runtime = Runtime::new(SilentProvider, model, 256).unwrap();
        let plan = CompiledAgentPlan::compile_blocking(AgentProfile::embedded(
            &runtime,
            workspace.to_owned(),
        ))
        .unwrap();
        let sources = plan.instruction_sources().to_vec();
        CompiledGeneration {
            plan,
            sources,
            configuration_sources: Vec::new(),
            bindings: LiveBindings::default(),
        }
    }

    fn canonical_temp() -> tempfile::TempDir {
        // Plans require canonical roots; macOS temp dirs are symlinks.
        let directory = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(directory.path()).unwrap();
        assert_eq!(
            canonical,
            directory.path(),
            "test temp dir must be canonical"
        );
        directory
    }

    #[test]
    fn warm_lookup_hits_without_compiling_and_edits_recompile() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits::default());
        let compiles = AtomicUsize::new(0);
        let load = || {
            cache.load::<std::convert::Infallible, _>(key(directory.path(), "m"), || {
                compiles.fetch_add(1, Ordering::SeqCst);
                Ok(compile(directory.path(), "m"))
            })
        };

        let (first, lookup) = load().unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        let (second, lookup) = load().unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(compiles.load(Ordering::SeqCst), 1);

        // Creating AGENTS.md changes an instruction source: recompile, new
        // digest, new generation; the old Arc stays valid for its holders.
        std::fs::write(directory.path().join("AGENTS.md"), "be terse\n").unwrap();
        let (third, lookup) = load().unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        assert!(!Arc::ptr_eq(&first, &third));
        assert_ne!(first.digest(), third.digest());
        assert_eq!(compiles.load(Ordering::SeqCst), 2);
        assert_eq!(cache.len(), 1);
        // The first generation is still usable by whoever holds it.
        assert_eq!(first.workspace_path(), directory.path());
    }

    #[test]
    fn source_change_with_identical_result_keeps_the_live_generation() {
        let directory = canonical_temp();
        let instructions = directory.path().join("AGENTS.md");
        std::fs::write(&instructions, "be terse\n").unwrap();
        let cache = PlanCache::new(PlanCacheLimits::default());
        let load = || {
            cache.load::<std::convert::Infallible, _>(key(directory.path(), "m"), || {
                Ok(compile(directory.path(), "m"))
            })
        };

        let (first, _) = load().unwrap();
        // Touch the file with identical content: the fingerprint changes but
        // the compiled behavior does not.
        thread::sleep(std::time::Duration::from_millis(20));
        let staged = directory.path().join("AGENTS.md.tmp");
        std::fs::write(&staged, "be terse\n").unwrap();
        std::fs::rename(&staged, &instructions).unwrap();
        let (second, lookup) = load().unwrap();
        assert_eq!(lookup, PlanLookup::Revalidated);
        assert!(Arc::ptr_eq(&first, &second));
        let (_, lookup) = load().unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
    }

    #[test]
    fn compile_failure_leaves_the_previous_generation_cached() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits::default());
        let (first, _) = cache
            .load::<String, _>(key(directory.path(), "m"), || {
                Ok(compile(directory.path(), "m"))
            })
            .unwrap();
        std::fs::write(directory.path().join("AGENTS.md"), "x").unwrap();
        let failed = cache.load::<String, _>(key(directory.path(), "m"), || {
            Err("configuration is broken".to_owned())
        });
        assert!(
            matches!(failed, Err(PlanCacheError::Compile(message)) if message.contains("broken"))
        );
        // Reverting the edit makes the recorded fingerprints current again.
        std::fs::remove_file(directory.path().join("AGENTS.md")).unwrap();
        let (again, lookup) = cache
            .load::<String, _>(key(directory.path(), "m"), || panic!("must not compile"))
            .unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[test]
    fn eviction_is_lru_among_inactive_generations_and_pinned_entries_survive() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits {
            max_entries: 2,
            max_bytes: usize::MAX,
        });
        let load = |model: &str| {
            cache
                .load::<std::convert::Infallible, _>(key(directory.path(), model), || {
                    Ok(compile(directory.path(), model))
                })
                .unwrap()
                .0
        };

        let pinned = load("a");
        drop(load("b"));
        // Touch "a" so "b" is the least recently used.
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(key(directory.path(), "a"), || unreachable!())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
        drop(load("c"));
        assert_eq!(cache.len(), 2);
        // "b" was evicted; "a" is still cached (and pinned by `pinned`).
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(key(directory.path(), "a"), || unreachable!())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(key(directory.path(), "b"), || {
                Ok(compile(directory.path(), "b"))
            })
            .unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        drop(pinned);
    }

    #[test]
    fn admission_fails_when_every_entry_is_pinned() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits {
            max_entries: 1,
            max_bytes: usize::MAX,
        });
        let pinned = cache
            .load::<std::convert::Infallible, _>(key(directory.path(), "a"), || {
                Ok(compile(directory.path(), "a"))
            })
            .unwrap()
            .0;
        let full = cache.load::<std::convert::Infallible, _>(key(directory.path(), "b"), || {
            Ok(compile(directory.path(), "b"))
        });
        assert!(matches!(
            full,
            Err(PlanCacheError::Capacity {
                active_entries: 1,
                ..
            })
        ));
        assert!(cache.estimated_bytes() > 0);
        drop(pinned);
        // Released, the entry is evictable and admission succeeds.
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(key(directory.path(), "b"), || {
                Ok(compile(directory.path(), "b"))
            })
            .unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
    }

    #[test]
    fn byte_limit_bounds_admission_like_the_entry_limit() {
        let directory = canonical_temp();
        let probe = compile(directory.path(), "a");
        let one_plan = probe.estimated_bytes();
        drop(probe);
        let cache = PlanCache::new(PlanCacheLimits {
            max_entries: usize::MAX,
            max_bytes: one_plan + one_plan / 2,
        });
        let first = cache
            .load::<std::convert::Infallible, _>(key(directory.path(), "a"), || {
                Ok(compile(directory.path(), "a"))
            })
            .unwrap()
            .0;
        let second = cache.load::<std::convert::Infallible, _>(key(directory.path(), "b"), || {
            Ok(compile(directory.path(), "b"))
        });
        assert!(matches!(second, Err(PlanCacheError::Capacity { .. })));
        drop(first);
        assert!(
            cache
                .load::<std::convert::Infallible, _>(key(directory.path(), "b"), || {
                    Ok(compile(directory.path(), "b"))
                })
                .is_ok()
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn concurrent_misses_on_one_key_compile_once() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits::default());
        let compiles = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let cache = cache.clone();
                let compiles = Arc::clone(&compiles);
                let workspace = directory.path().to_owned();
                thread::spawn(move || {
                    cache
                        .load::<std::convert::Infallible, _>(key(&workspace, "m"), || {
                            compiles.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(std::time::Duration::from_millis(10));
                            Ok(compile(&workspace, "m"))
                        })
                        .unwrap()
                        .0
                })
            })
            .collect();
        let plans: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(compiles.load(Ordering::SeqCst), 1);
        assert!(plans.iter().all(|plan| Arc::ptr_eq(plan, &plans[0])));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn shutdown_drops_generations_and_refuses_loads() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits::default());
        let held = cache
            .load::<std::convert::Infallible, _>(key(directory.path(), "m"), || {
                Ok(compile(directory.path(), "m"))
            })
            .unwrap()
            .0;
        cache.shutdown();
        assert!(cache.is_empty());
        assert!(matches!(
            cache
                .load::<std::convert::Infallible, _>(key(directory.path(), "m"), || unreachable!()),
            Err(PlanCacheError::ShutDown)
        ));
        // The plan a run holds is unaffected.
        assert_eq!(held.workspace_path(), directory.path());
    }
}
