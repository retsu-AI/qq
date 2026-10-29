use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use qq_config::{AwsAuth, BedrockAuth, HttpCredential, ProviderAccess, ProviderAuth};
use qq_core::plan::{CompiledAgentPlan, SourceFingerprint};
use qq_protocol::{AgentPlanDigest, AgentProfileId, CredentialEpoch, ModelSelection};
use qq_provider::SecretRef;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCacheLimits {
    pub max_entries: usize,
    pub max_bytes: usize,
}

impl Default for PlanCacheLimits {
    fn default() -> Self {
        Self {
            max_entries: 16,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PlanKey {
    pub configuration_workspace: PathBuf,
    pub workspace: PathBuf,
    pub model: ModelSelection,
    pub profile: AgentProfileId,
    pub explicit_config_path: Option<PathBuf>,
    pub explicit_config_content: Option<String>,
    pub reasoning_effort: Option<qq_provider::ReasoningEffort>,
    pub ignore_untrusted_project_config: bool,
}

impl std::fmt::Debug for PlanKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlanKey")
            .field("workspace", &self.workspace)
            .field("configuration_workspace", &self.configuration_workspace)
            .field("model", &self.model)
            .field("profile", &self.profile)
            .field("explicit_config_path", &self.explicit_config_path)
            .field(
                "explicit_config_content",
                &self.explicit_config_content.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "ignore_untrusted_project_config",
                &self.ignore_untrusted_project_config,
            )
            .finish()
    }
}

pub struct CompiledGeneration {
    pub plan: Arc<CompiledAgentPlan>,
    pub configuration_sources: Vec<qq_config::ConfigSources>,
    pub sources: Vec<SourceFingerprint>,
    pub bindings: LiveBindings,
}

#[derive(Default)]
pub struct LiveBindings {
    pub provider: Option<ProviderAccess>,
    pub mcp: Option<Arc<crate::mcp::WiredMcpRegistry>>,
    pub runtime: RuntimeSwitches,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeSwitches {
    pub output_ceiling: Option<qq_core::OutputCeiling>,
}

impl PartialEq for LiveBindings {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.runtime == other.runtime
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
                    HttpCredential::Builtin {
                        api_key, profile, ..
                    } => {
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
            .saturating_add(self.sources.capacity() * std::mem::size_of::<SourceFingerprint>())
            .saturating_add(
                self.sources
                    .iter()
                    .map(|fingerprint| fingerprint.path().as_os_str().len())
                    .sum::<usize>(),
            )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLookup {
    Hit,
    Revalidated,
    Compiled,
}

struct Slot {
    key: PlanKey,
    generation: CompiledGeneration,
    digest: AgentPlanDigest,
    epoch: CredentialEpoch,
}

struct State {
    slots: VecDeque<Slot>,
    superseded: Vec<CompiledGeneration>,
    shut_down: bool,
}

#[derive(Clone)]
pub struct PlanCache {
    inner: Arc<Inner>,
}

struct Inner {
    limits: PlanCacheLimits,
    state: Mutex<State>,
    in_flight: Mutex<Vec<(PlanKey, Arc<Mutex<()>>)>>,
}

impl PlanCache {
    #[must_use]
    pub fn new(limits: PlanCacheLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                limits,
                state: Mutex::new(State {
                    slots: VecDeque::new(),
                    superseded: Vec::new(),
                    shut_down: false,
                }),
                in_flight: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn load<E, F>(
        &self,
        key: &PlanKey,
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
            match in_flight.iter().find(|(flight_key, _)| flight_key == key) {
                Some((_, guard)) => Arc::clone(guard),
                None => {
                    let guard = Arc::new(Mutex::new(()));
                    in_flight.push((key.clone(), Arc::clone(&guard)));
                    guard
                }
            }
        };
        let outcome = {
            let _flight = flight.lock().map_err(|_| PlanCacheError::Poisoned)?;
            self.load_under_flight(key, compile)
        };
        if let Ok(mut in_flight) = self.inner.in_flight.lock()
            && Arc::strong_count(&flight) == 2
            && let Some(index) = in_flight
                .iter()
                .position(|(flight_key, _)| flight_key == key)
        {
            in_flight.swap_remove(index);
        }
        outcome
    }

    fn load_under_flight<E, F>(
        &self,
        key: &PlanKey,
        compile: F,
    ) -> Result<(Arc<CompiledAgentPlan>, PlanLookup), PlanCacheError<E>>
    where
        F: FnOnce() -> Result<CompiledGeneration, E>,
    {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| PlanCacheError::Poisoned)?;
            if state.shut_down {
                return Err(PlanCacheError::ShutDown);
            }
            if let Some(index) = state.slots.iter().position(|slot| slot.key == *key) {
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
        let existing = state.slots.iter().position(|slot| slot.key == *key);
        if let Some(index) = existing {
            let slot = &state.slots[index];
            if slot.digest == digest
                && slot.epoch == epoch
                && slot.generation.bindings == generation.bindings
            {
                let growth = generation
                    .estimated_bytes()
                    .saturating_sub(slot.generation.estimated_bytes());
                admit(&mut state, self.inner.limits, growth, Some(key))?;
                let index = state
                    .slots
                    .iter()
                    .position(|slot| slot.key == *key)
                    .expect("the replaced slot is never evicted");
                let mut slot = state
                    .slots
                    .remove(index)
                    .expect("a located slot must exist");
                slot.generation.sources = generation.sources;
                slot.generation.configuration_sources = generation.configuration_sources;
                let plan = Arc::clone(&slot.generation.plan);
                state.slots.push_back(slot);
                return Ok((plan, PlanLookup::Revalidated));
            }
        }

        admit(
            &mut state,
            self.inner.limits,
            generation.estimated_bytes(),
            Some(key),
        )?;
        if let Some(index) = state.slots.iter().position(|slot| slot.key == *key) {
            let old = state
                .slots
                .remove(index)
                .expect("a located slot must exist");
            if Arc::strong_count(&old.generation.plan) > 1 {
                state.superseded.push(old.generation);
            }
        }
        let plan = Arc::clone(&generation.plan);
        state.slots.push_back(Slot {
            key: key.clone(),
            generation,
            digest,
            epoch,
        });
        Ok((plan, PlanLookup::Compiled))
    }

    pub fn shutdown(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.shut_down = true;
            state.slots.clear();
            state.superseded.clear();
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

    #[cfg(test)]
    fn superseded_len(&self) -> usize {
        self.inner.state.lock().map_or(0, |mut state| {
            release_settled(&mut state);
            state.superseded.len()
        })
    }

    #[cfg(test)]
    fn in_flight_len(&self) -> usize {
        self.inner.in_flight.lock().map_or(0, |guards| guards.len())
    }

    #[cfg(test)]
    fn estimated_bytes(&self) -> usize {
        self.inner.state.lock().map_or(0, |mut state| {
            release_settled(&mut state);
            state
                .slots
                .iter()
                .map(|slot| slot.generation.estimated_bytes())
                .chain(
                    state
                        .superseded
                        .iter()
                        .map(CompiledGeneration::estimated_bytes),
                )
                .sum()
        })
    }
}

fn release_settled(state: &mut State) {
    state
        .superseded
        .retain(|generation| Arc::strong_count(&generation.plan) > 1);
}

fn admit<E>(
    state: &mut State,
    limits: PlanCacheLimits,
    requested_bytes: usize,
    replacing: Option<&PlanKey>,
) -> Result<(), PlanCacheError<E>> {
    release_settled(state);
    let superseded_entries = state.superseded.len();
    let superseded_bytes: usize = state
        .superseded
        .iter()
        .map(CompiledGeneration::estimated_bytes)
        .sum();
    loop {
        let counted = |slot: &Slot| {
            replacing != Some(&slot.key) || Arc::strong_count(&slot.generation.plan) > 1
        };
        let (entries, used_bytes) = state.slots.iter().filter(|slot| counted(slot)).fold(
            (superseded_entries, superseded_bytes),
            |(entries, bytes), slot| {
                (
                    entries + 1,
                    bytes.saturating_add(slot.generation.estimated_bytes()),
                )
            },
        );
        let fits = entries < limits.max_entries
            && used_bytes.saturating_add(requested_bytes) <= limits.max_bytes;
        if fits {
            return Ok(());
        }
        let evictable = state.slots.iter().position(|slot| {
            Arc::strong_count(&slot.generation.plan) == 1 && replacing != Some(&slot.key)
        });
        match evictable {
            Some(index) => {
                state.slots.remove(index);
            }
            None => {
                return Err(PlanCacheError::Capacity {
                    active_entries: entries,
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
            configuration_workspace: workspace.to_owned(),
            workspace: workspace.to_owned(),
            model: ModelSelection {
                model_is_fallback: false,
                model: Some(model.to_owned()),
                max_output_tokens: None,
                organization: None,
            },
            profile: AgentProfileId::default(),
            explicit_config_path: None,
            explicit_config_content: None,
            reasoning_effort: None,
            ignore_untrusted_project_config: false,
        }
    }

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
            cache.load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || {
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

        std::fs::write(directory.path().join("AGENTS.md"), "be terse\n").unwrap();
        let (third, lookup) = load().unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        assert!(!Arc::ptr_eq(&first, &third));
        assert_ne!(first.digest(), third.digest());
        assert_eq!(compiles.load(Ordering::SeqCst), 2);
        assert_eq!(cache.len(), 1);
        assert_eq!(first.workspace_path(), directory.path());
        assert_eq!(cache.superseded_len(), 1);
        drop(first);
        drop(second);
        assert_eq!(cache.superseded_len(), 0);
    }

    #[test]
    fn plan_key_debug_redacts_inline_configuration() {
        let mut key = key(Path::new("/w"), "m");
        key.explicit_config_content = Some("auth: ApiKey(\"sk-live-secret\")".to_owned());
        let rendered = format!("{key:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("sk-live"), "{rendered}");
    }

    #[test]
    fn same_key_refresh_keeps_a_pinned_predecessor_in_the_accounting() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits {
            max_entries: 2,
            max_bytes: usize::MAX,
        });
        let load = |model: &str| {
            cache.load::<std::convert::Infallible, _>(&key(directory.path(), model), || {
                Ok(compile(directory.path(), model))
            })
        };
        let (first, _) = load("m").unwrap();
        let one_plan = cache.estimated_bytes();
        std::fs::write(directory.path().join("AGENTS.md"), "be terse\n").unwrap();
        let (second, lookup) = load("m").unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.superseded_len(), 1);
        assert!(cache.estimated_bytes() > one_plan);
        assert!(matches!(
            load("other"),
            Err(PlanCacheError::Capacity {
                active_entries: 2,
                ..
            })
        ));
        drop(first);
        assert_eq!(load("other").unwrap().1, PlanLookup::Compiled);
        assert_eq!(cache.superseded_len(), 0);
        drop(second);
    }

    #[test]
    fn rejected_replacement_keeps_the_previous_generation() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits {
            max_entries: 1,
            max_bytes: usize::MAX,
        });
        let load = || {
            cache.load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || {
                Ok(compile(directory.path(), "m"))
            })
        };
        let (first, _) = load().unwrap();
        std::fs::write(directory.path().join("AGENTS.md"), "be terse\n").unwrap();
        assert!(matches!(load(), Err(PlanCacheError::Capacity { .. })));
        assert_eq!(cache.len(), 1);
        std::fs::remove_file(directory.path().join("AGENTS.md")).unwrap();
        let (again, lookup) = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || {
                panic!("must not compile")
            })
            .unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
        assert!(Arc::ptr_eq(&first, &again));
        drop(again);
        drop(first);
        std::fs::write(directory.path().join("AGENTS.md"), "be terse\n").unwrap();
        assert_eq!(load().unwrap().1, PlanLookup::Compiled);
        assert_eq!(cache.superseded_len(), 0);
    }

    fn write_newer(path: &std::path::Path, than: &std::path::Path, contents: &str) {
        let previous = std::fs::metadata(than).unwrap().modified().unwrap();
        std::fs::write(path, contents).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(previous + std::time::Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn equivalent_refresh_admits_grown_source_evidence() {
        let directory = canonical_temp();
        let instructions = directory.path().join("AGENTS.md");
        std::fs::write(&instructions, "be terse\n").unwrap();
        let probe = compile(directory.path(), "m");
        let one_plan = probe.estimated_bytes();
        drop(probe);
        let cache = PlanCache::new(PlanCacheLimits {
            max_entries: usize::MAX,
            max_bytes: one_plan + 2048,
        });
        let padded = |count: usize| {
            let mut generation = compile(directory.path(), "m");
            for index in 0..count {
                generation.sources.push(SourceFingerprint::capture(
                    directory.path().join(format!("absent-{index:04}")),
                ));
            }
            generation
        };
        let (first, _) = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || Ok(padded(0)))
            .unwrap();
        let touch = || {
            let staged = directory.path().join("AGENTS.md.tmp");
            write_newer(&staged, &instructions, "be terse\n");
            std::fs::rename(&staged, &instructions).unwrap();
        };
        touch();
        let grown = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || Ok(padded(64)));
        assert!(matches!(grown, Err(PlanCacheError::Capacity { .. })));
        assert_eq!(cache.len(), 1);
        assert!(cache.estimated_bytes() <= one_plan + 2048);
        let (again, lookup) = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || Ok(padded(0)))
            .unwrap();
        assert_eq!(lookup, PlanLookup::Revalidated);
        assert!(Arc::ptr_eq(&first, &again));
        touch();
        let before = cache.estimated_bytes();
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || Ok(padded(1)))
            .unwrap();
        assert_eq!(lookup, PlanLookup::Revalidated);
        assert!(cache.estimated_bytes() > before);
    }

    #[test]
    fn completed_compile_guards_are_reclaimed_under_distinct_key_churn() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits {
            max_entries: 2,
            max_bytes: usize::MAX,
        });
        for index in 0..32 {
            let model = format!("m{index}");
            drop(
                cache
                    .load::<std::convert::Infallible, _>(&key(directory.path(), &model), || {
                        Ok(compile(directory.path(), &model))
                    })
                    .unwrap(),
            );
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.in_flight_len(), 0);
    }

    #[test]
    fn source_change_with_identical_result_keeps_the_live_generation() {
        let directory = canonical_temp();
        let instructions = directory.path().join("AGENTS.md");
        std::fs::write(&instructions, "be terse\n").unwrap();
        let cache = PlanCache::new(PlanCacheLimits::default());
        let load = || {
            cache.load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || {
                Ok(compile(directory.path(), "m"))
            })
        };

        let (first, _) = load().unwrap();
        let staged = directory.path().join("AGENTS.md.tmp");
        write_newer(&staged, &instructions, "be terse\n");
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
            .load::<String, _>(&key(directory.path(), "m"), || {
                Ok(compile(directory.path(), "m"))
            })
            .unwrap();
        std::fs::write(directory.path().join("AGENTS.md"), "x").unwrap();
        let failed = cache.load::<String, _>(&key(directory.path(), "m"), || {
            Err("configuration is broken".to_owned())
        });
        assert!(
            matches!(failed, Err(PlanCacheError::Compile(message)) if message.contains("broken"))
        );
        std::fs::remove_file(directory.path().join("AGENTS.md")).unwrap();
        let (again, lookup) = cache
            .load::<String, _>(&key(directory.path(), "m"), || panic!("must not compile"))
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
                .load::<std::convert::Infallible, _>(&key(directory.path(), model), || {
                    Ok(compile(directory.path(), model))
                })
                .unwrap()
                .0
        };

        let pinned = load("a");
        drop(load("b"));
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "a"), || unreachable!())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
        drop(load("c"));
        assert_eq!(cache.len(), 2);
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "a"), || unreachable!())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "b"), || {
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
            .load::<std::convert::Infallible, _>(&key(directory.path(), "a"), || {
                Ok(compile(directory.path(), "a"))
            })
            .unwrap()
            .0;
        let full = cache.load::<std::convert::Infallible, _>(&key(directory.path(), "b"), || {
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
        let (_, lookup) = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "b"), || {
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
            .load::<std::convert::Infallible, _>(&key(directory.path(), "a"), || {
                Ok(compile(directory.path(), "a"))
            })
            .unwrap()
            .0;
        let second = cache.load::<std::convert::Infallible, _>(&key(directory.path(), "b"), || {
            Ok(compile(directory.path(), "b"))
        });
        assert!(matches!(second, Err(PlanCacheError::Capacity { .. })));
        drop(first);
        assert!(
            cache
                .load::<std::convert::Infallible, _>(&key(directory.path(), "b"), || {
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
        let callers = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let cache = cache.clone();
                let compiles = Arc::clone(&compiles);
                let callers = Arc::clone(&callers);
                let workspace = directory.path().to_owned();
                thread::spawn(move || {
                    callers.fetch_add(1, Ordering::SeqCst);
                    cache
                        .load::<std::convert::Infallible, _>(&key(&workspace, "m"), || {
                            compiles.fetch_add(1, Ordering::SeqCst);
                            while callers.load(Ordering::SeqCst) < 16 {
                                thread::yield_now();
                            }
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
        assert_eq!(cache.in_flight_len(), 0);
    }

    #[test]
    fn shutdown_drops_generations_and_refuses_loads() {
        let directory = canonical_temp();
        let cache = PlanCache::new(PlanCacheLimits::default());
        let held = cache
            .load::<std::convert::Infallible, _>(&key(directory.path(), "m"), || {
                Ok(compile(directory.path(), "m"))
            })
            .unwrap()
            .0;
        cache.shutdown();
        assert!(cache.is_empty());
        assert!(matches!(
            cache.load::<std::convert::Infallible, _>(
                &key(directory.path(), "m"),
                || unreachable!()
            ),
            Err(PlanCacheError::ShutDown)
        ));
        assert_eq!(held.workspace_path(), directory.path());
    }
}
