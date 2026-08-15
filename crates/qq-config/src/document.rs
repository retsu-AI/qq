use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    marker::PhantomData,
};

use ron::{Options, extensions::Extensions};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, EnumAccess, MapAccess, VariantAccess, Visitor},
};
use sha2::{Digest, Sha256};

use super::{
    AwsAuth, BedrockAuth, ConfigError, ConfigKey, ConfigProvenance, ConfigSnapshot, Connection,
    DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MCP_CALL_TIMEOUT_SECONDS, DEFAULT_MCP_MAX_CONCURRENT_CALLS,
    EffectivePolicy, HttpAccess, HttpCredential, InputModality, MAX_MCP_CALL_TIMEOUT_SECONDS,
    MAX_MCP_MAX_CONCURRENT_CALLS, McpServerConfig, McpTransport, ModelMetadata, ModelPricing,
    ModelRoute, PolicyGrants, ProviderAccess, ProviderApi, ProviderConfig, ProviderKind,
    RuntimeOverrides, SecretRef, SourceIdentity, SourceKind, SourceReport, WorkspaceGrant,
};

pub(super) fn deserialize_unique_btree_map<'de, D, K, V>(
    deserializer: D,
) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Ord + fmt::Debug,
    V: Deserialize<'de>,
{
    struct UniqueMapVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> Visitor<'de> for UniqueMapVisitor<K, V>
    where
        K: Deserialize<'de> + Ord + fmt::Debug,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map without duplicate keys")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = access.next_entry()? {
                if values.contains_key(&key) {
                    return Err(de::Error::custom(format_args!("duplicate map key {key:?}")));
                }
                values.insert(key, value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct UniqueMap<K: Ord, V>(BTreeMap<K, V>);

impl<'de, K, V> Deserialize<'de> for UniqueMap<K, V>
where
    K: Deserialize<'de> + Ord + fmt::Debug,
    V: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_unique_btree_map(deserializer).map(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ClearMarker {
    Clear,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Field<T> {
    #[default]
    Missing,
    Set(T),
    Clear,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum StringField {
    #[default]
    Missing,
    Set(String),
    Clear,
}

impl StringField {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    fn is_present(&self) -> bool {
        !self.is_missing()
    }
}

impl Serialize for StringField {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Set(value) => serializer.serialize_newtype_variant("Patch", 0, "Set", value),
            Self::Clear => serializer.serialize_unit_variant("Patch", 1, "Clear"),
            Self::Missing => serializer.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for StringField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StringVisitor;

        impl<'de> Visitor<'de> for StringVisitor {
            type Value = StringField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a quoted string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringField::Set(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringField::Set(value))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringField::Clear)
            }

            fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
            where
                A: EnumAccess<'de>,
            {
                let (variant, access) = data.variant::<String>()?;
                if variant != "Clear" {
                    return Err(de::Error::unknown_variant(&variant, &["Clear"]));
                }
                access.unit_variant()?;
                Ok(StringField::Clear)
            }
        }

        deserializer.deserialize_any(StringVisitor)
    }
}

impl<T> Field<T> {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    fn is_present(&self) -> bool {
        !self.is_missing()
    }
}

impl<T: Serialize> Serialize for Field<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Set(value) => serializer.serialize_newtype_variant("Patch", 0, "Set", value),
            Self::Clear => serializer.serialize_unit_variant("Patch", 1, "Clear"),
            Self::Missing => serializer.serialize_unit(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Present<T> {
            Clear(ClearMarker),
            Set(T),
        }

        match Present::deserialize(deserializer)? {
            Present::Clear(ClearMarker::Clear) => Ok(Self::Clear),
            Present::Set(value) => Ok(Self::Set(value)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum RemoveMarker {
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum ModelEntryPatch {
    Set(ModelPatch),
    Remove(RemoveMarker),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ModelPatch {
    #[serde(skip_serializing_if = "StringField::is_missing")]
    name: StringField,
    #[serde(skip_serializing_if = "Field::is_missing")]
    api: Field<ProviderApi>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    reasoning: Field<bool>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    input: Field<Vec<InputModality>>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    context_window: Field<u32>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    max_output_tokens: Field<u32>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    pricing: Field<ModelPricing>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum ProviderEntryPatch {
    OpenAi {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api_key: Field<SecretRef>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    OpenAiCodex {
        #[serde(default, skip_serializing_if = "StringField::is_missing")]
        profile: StringField,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    Anthropic {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api_key: Field<SecretRef>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    Google {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api_key: Field<SecretRef>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    XAi {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api_key: Field<SecretRef>,
        #[serde(default, skip_serializing_if = "StringField::is_missing")]
        profile: StringField,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    LiteLlm {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        connection: Field<Connection>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    AmazonBedrock {
        #[serde(default, skip_serializing_if = "StringField::is_missing")]
        region: StringField,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        auth: Field<BedrockAuth>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    AmazonBedrockMantle {
        #[serde(default, skip_serializing_if = "StringField::is_missing")]
        region: StringField,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api: Field<ProviderApi>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        auth: Field<BedrockAuth>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    Custom {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        connection: Field<Connection>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    Remove,
}

impl ProviderEntryPatch {
    pub(super) fn contains_literal_secret(&self) -> bool {
        match self {
            Self::OpenAi { api_key, .. }
            | Self::Anthropic { api_key, .. }
            | Self::Google { api_key, .. }
            | Self::XAi { api_key, .. } => {
                matches!(api_key, Field::Set(SecretRef::Value(_)))
            }
            Self::LiteLlm { connection, .. } | Self::Custom { connection, .. } => {
                matches!(connection, Field::Set(value) if value.contains_literal_secret())
            }
            Self::AmazonBedrock { auth, .. } | Self::AmazonBedrockMantle { auth, .. } => {
                matches!(auth, Field::Set(value) if value.contains_literal_secret())
            }
            Self::OpenAiCodex { .. } | Self::Remove => false,
        }
    }

    fn references_local_credential(&self) -> bool {
        match self {
            Self::OpenAi { api_key, .. }
            | Self::Anthropic { api_key, .. }
            | Self::Google { api_key, .. } => matches!(api_key, Field::Set(_)),
            Self::XAi {
                api_key, profile, ..
            } => matches!(api_key, Field::Set(_)) || matches!(profile, StringField::Set(_)),
            Self::OpenAiCodex { profile, .. } => matches!(profile, StringField::Set(_)),
            Self::LiteLlm { connection, .. } | Self::Custom { connection, .. } => {
                matches!(
                    connection,
                    Field::Set(value) if value.references_local_credential()
                )
            }
            Self::AmazonBedrock { auth, .. } | Self::AmazonBedrockMantle { auth, .. } => {
                matches!(auth, Field::Set(value) if value.references_local_credential())
            }
            Self::Remove => false,
        }
    }
}

/// One declared MCP server. Entries replace whole declarations by name
/// (unlike provider patches there is no per-field layering) and `Remove`
/// deletes a server declared by an earlier layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum McpServerPatch {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<String>,
        #[serde(default)]
        eager: bool,
        #[serde(default)]
        allow: Vec<String>,
        #[serde(default)]
        call_timeout_seconds: Option<u64>,
        #[serde(default)]
        max_concurrent_calls: Option<u32>,
    },
    Http {
        url: String,
        #[serde(default)]
        bearer: Option<SecretRef>,
        #[serde(default)]
        eager: bool,
        #[serde(default)]
        allow: Vec<String>,
        #[serde(default)]
        call_timeout_seconds: Option<u64>,
        #[serde(default)]
        max_concurrent_calls: Option<u32>,
    },
    Remove,
}

impl McpServerPatch {
    fn contains_literal_secret(&self) -> bool {
        matches!(
            self,
            Self::Http {
                bearer: Some(SecretRef::Value(_)),
                ..
            }
        )
    }
}

/// Marks one grant removed from the set accumulated by earlier layers, the
/// same idiom `mcp`/`providers` use, spelled `Remove("name")` in RON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum GrantRemoval {
    Remove(String),
}

/// One entry in a `policy` grant list: a plain string adds the grant, and
/// `Remove("name")` deletes a grant declared by an earlier layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum GrantEntry {
    Remove(GrantRemoval),
    Allow(String),
}

impl GrantEntry {
    fn name(&self) -> &str {
        match self {
            Self::Allow(name) | Self::Remove(GrantRemoval::Remove(name)) => name,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyPatch {
    allowed_providers: Option<Vec<String>>,
    denied_providers: Option<Vec<String>>,
    max_output_tokens: Option<u32>,
    require_https: Option<bool>,
    allow_custom_providers: Option<bool>,
    allow_literal_secrets: Option<bool>,
    /// Approval grants: exact tool names that run without prompting.
    /// Declarable by ordinary (workspace/user) sources.
    allow_tools: Option<Vec<GrantEntry>>,
    /// Approval grants: shell command prefixes matched at word granularity.
    /// Declarable by ordinary (workspace/user) sources.
    allow_shell_prefixes: Option<Vec<GrantEntry>>,
    /// Managed-only: exact tool names filtered out of the effective grant set
    /// no matter which lower layer declared them.
    deny_tools: Option<Vec<String>>,
    /// Managed-only: shell prefixes whose word-granularity overlap filters
    /// lower-layer shell grants out of the effective grant set.
    deny_shell_prefixes: Option<Vec<String>>,
}

impl PolicyPatch {
    /// Fields only administrator-controlled (Managed/MDM) sources may set.
    fn has_managed_only_fields(&self) -> bool {
        self.allowed_providers.is_some()
            || self.denied_providers.is_some()
            || self.max_output_tokens.is_some()
            || self.require_https.is_some()
            || self.allow_custom_providers.is_some()
            || self.allow_literal_secrets.is_some()
            || self.deny_tools.is_some()
            || self.deny_shell_prefixes.is_some()
    }

    /// Approval grants any source may declare, gated by workspace trust.
    fn has_grants(&self) -> bool {
        self.allow_tools.is_some() || self.allow_shell_prefixes.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Document {
    version: u32,
    #[serde(default, skip_serializing_if = "StringField::is_missing")]
    organization: StringField,
    #[serde(default, skip_serializing_if = "StringField::is_missing")]
    model: StringField,
    #[serde(default, skip_serializing_if = "StringField::is_missing")]
    worker_model: StringField,
    #[serde(default, skip_serializing_if = "StringField::is_missing")]
    reviewer_model: StringField,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    max_output_tokens: Field<u32>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    providers: Field<UniqueMap<String, ProviderEntryPatch>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    mcp: Field<UniqueMap<String, McpServerPatch>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<PolicyPatch>,
}

impl Document {
    pub(super) fn parse(content: &str, origin: &SourceIdentity) -> Result<Self, ConfigError> {
        let options = Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
        let document: Self = options
            .from_str(content)
            .map_err(|error| ConfigError::Parse {
                origin: origin.clone(),
                message: error.to_string(),
            })?;
        if document.version != 1 {
            return Err(ConfigError::UnsupportedVersion {
                origin: origin.clone(),
                version: document.version,
            });
        }
        document.validate(origin)?;
        Ok(document)
    }

    fn validate(&self, origin: &SourceIdentity) -> Result<(), ConfigError> {
        if let Some(policy) = &self.policy {
            if policy.has_managed_only_fields()
                && !matches!(origin.kind(), SourceKind::Managed | SourceKind::Mdm)
            {
                return Err(ConfigError::PolicyOutsideManaged {
                    origin: origin.clone(),
                });
            }
            // Approval grants name commands and tools that run without
            // prompting; remote configuration may never plant them, exactly
            // like MCP declarations.
            if origin.kind() == SourceKind::Remote && policy.has_grants() {
                return Err(ConfigError::RemotePolicyGrantsForbidden {
                    origin: origin.clone(),
                });
            }
        }
        if self.contains_literal_secret()
            && !matches!(
                origin.kind(),
                SourceKind::Global | SourceKind::Explicit | SourceKind::Inline
            )
        {
            return Err(ConfigError::LiteralSecretForbidden {
                origin: origin.clone(),
            });
        }
        if origin.kind() == SourceKind::Remote && self.references_local_credential() {
            return Err(ConfigError::RemoteCredentialReferenceForbidden {
                origin: origin.clone(),
            });
        }
        // MCP declarations name commands to execute and endpoints to call;
        // remote configuration may never plant them.
        if origin.kind() == SourceKind::Remote && self.mcp.is_present() {
            return Err(ConfigError::RemoteMcpForbidden {
                origin: origin.clone(),
            });
        }
        if let Field::Set(servers) = &self.mcp {
            validate_mcp_servers(servers, origin)?;
        }
        if let Some(policy) = &self.policy {
            validate_policy_names(policy, origin)?;
        }
        Ok(())
    }

    pub(super) fn has_sensitive_operations(&self) -> bool {
        self.organization.is_present()
            || self.model.is_present()
            || self.worker_model.is_present()
            || self.reviewer_model.is_present()
            || self.providers.is_present()
            || self.mcp.is_present()
            || self.policy.as_ref().is_some_and(PolicyPatch::has_grants)
    }

    pub(super) fn sensitive_digest(&self) -> Result<Option<String>, ConfigError> {
        if !self.has_sensitive_operations() {
            return Ok(None);
        }

        #[derive(Serialize)]
        struct GrantsProjection<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            allow_tools: Option<&'a Vec<GrantEntry>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            allow_shell_prefixes: Option<&'a Vec<GrantEntry>>,
        }

        #[derive(Serialize)]
        struct SensitiveProjection<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            organization: Option<&'a StringField>,
            #[serde(skip_serializing_if = "Option::is_none")]
            model: Option<&'a StringField>,
            #[serde(skip_serializing_if = "Option::is_none")]
            worker_model: Option<&'a StringField>,
            #[serde(skip_serializing_if = "Option::is_none")]
            reviewer_model: Option<&'a StringField>,
            #[serde(skip_serializing_if = "Option::is_none")]
            providers: Option<&'a Field<UniqueMap<String, ProviderEntryPatch>>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            mcp: Option<&'a Field<UniqueMap<String, McpServerPatch>>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            policy_grants: Option<GrantsProjection<'a>>,
        }

        fn present<T>(field: &Field<T>) -> Option<&Field<T>> {
            field.is_present().then_some(field)
        }

        let projection = SensitiveProjection {
            organization: self.organization.is_present().then_some(&self.organization),
            model: self.model.is_present().then_some(&self.model),
            worker_model: self.worker_model.is_present().then_some(&self.worker_model),
            reviewer_model: self
                .reviewer_model
                .is_present()
                .then_some(&self.reviewer_model),
            providers: present(&self.providers),
            mcp: present(&self.mcp),
            policy_grants: self
                .policy
                .as_ref()
                .filter(|policy| policy.has_grants())
                .map(|policy| GrantsProjection {
                    allow_tools: policy.allow_tools.as_ref(),
                    allow_shell_prefixes: policy.allow_shell_prefixes.as_ref(),
                }),
        };
        let canonical =
            serde_json::to_vec(&projection).map_err(|error| ConfigError::StateSerialization {
                message: error.to_string(),
            })?;
        let digest = Sha256::digest(canonical);
        let mut encoded = String::with_capacity(64);
        for byte in digest {
            use fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(Some(encoded))
    }

    pub(super) fn touched(&self) -> Vec<ConfigKey> {
        let mut touched = Vec::new();
        if self.organization.is_present() {
            touched.push(ConfigKey::Organization);
        }
        if self.model.is_present() {
            touched.push(ConfigKey::Model);
        }
        if self.worker_model.is_present() {
            touched.push(ConfigKey::WorkerModel);
        }
        if self.reviewer_model.is_present() {
            touched.push(ConfigKey::ReviewerModel);
        }
        if self.max_output_tokens.is_present() {
            touched.push(ConfigKey::MaxOutputTokens);
        }
        if self.providers.is_present() {
            touched.push(ConfigKey::Providers);
            if let Field::Set(providers) = &self.providers {
                touched.extend(providers.0.keys().cloned().map(ConfigKey::Provider));
            }
        }
        if self.mcp.is_present() {
            touched.push(ConfigKey::Mcp);
            if let Field::Set(servers) = &self.mcp {
                touched.extend(servers.0.keys().cloned().map(ConfigKey::McpServer));
            }
        }
        if self.policy.is_some() {
            touched.push(ConfigKey::Policy);
        }
        touched
    }

    pub(super) fn contains_literal_secret(&self) -> bool {
        let providers = match &self.providers {
            Field::Set(providers) => providers
                .0
                .values()
                .any(ProviderEntryPatch::contains_literal_secret),
            Field::Missing | Field::Clear => false,
        };
        let mcp = match &self.mcp {
            Field::Set(servers) => servers
                .0
                .values()
                .any(McpServerPatch::contains_literal_secret),
            Field::Missing | Field::Clear => false,
        };
        providers || mcp
    }

    fn references_local_credential(&self) -> bool {
        match &self.providers {
            Field::Set(providers) => providers
                .0
                .values()
                .any(ProviderEntryPatch::references_local_credential),
            Field::Missing | Field::Clear => false,
        }
    }

    pub(super) fn apply_organization(&self, organization: &mut Option<String>) -> bool {
        apply_optional_string(&self.organization, organization)
    }

    pub(super) fn matches_organization(&self, name: &str) -> bool {
        matches!(&self.organization, StringField::Set(value) if value == name)
    }

    /// Whether this document's own `policy` entries net out to declaring the
    /// grant: an `Allow` not undone by a later `Remove` in the same list.
    pub(super) fn declares_policy_grant(&self, grant: &WorkspaceGrant) -> bool {
        let Some(policy) = &self.policy else {
            return false;
        };
        let (entries, value) = match grant {
            WorkspaceGrant::Tool(name) => (policy.allow_tools.as_ref(), name),
            WorkspaceGrant::ShellPrefix(prefix) => (policy.allow_shell_prefixes.as_ref(), prefix),
        };
        let Some(entries) = entries else {
            return false;
        };
        let mut present = false;
        for entry in entries {
            match entry {
                GrantEntry::Allow(name) if name == value => present = true,
                GrantEntry::Remove(GrantRemoval::Remove(name)) if name == value => present = false,
                _ => {}
            }
        }
        present
    }

    /// Accumulates this document's managed deny lists for the promotion
    /// writer's refusal check.
    pub(super) fn collect_policy_denies(
        &self,
        deny_tools: &mut Vec<String>,
        deny_shell_prefixes: &mut Vec<String>,
    ) {
        let Some(policy) = &self.policy else {
            return;
        };
        if let Some(tools) = &policy.deny_tools {
            deny_tools.extend(tools.iter().cloned());
        }
        if let Some(prefixes) = &policy.deny_shell_prefixes {
            deny_shell_prefixes.extend(prefixes.iter().cloned());
        }
    }
}

const MAX_MCP_SERVER_NAME_BYTES: usize = 64;

/// Server names must keep the `mcp__<server>__<tool>` grammar unambiguous:
/// ASCII letters, digits, hyphens, or underscores, and never `__`.
fn valid_mcp_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_MCP_SERVER_NAME_BYTES
        && !name.contains("__")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn validate_mcp_servers(
    servers: &UniqueMap<String, McpServerPatch>,
    origin: &SourceIdentity,
) -> Result<(), ConfigError> {
    let invalid = |message: String| ConfigError::Parse {
        origin: origin.clone(),
        message,
    };
    for (name, patch) in &servers.0 {
        if !valid_mcp_server_name(name) {
            return Err(invalid(format!(
                "mcp server name {name:?} is invalid; use 1-{MAX_MCP_SERVER_NAME_BYTES} ASCII \
                 letters, digits, hyphens, or single underscores (`__` is the tool namespace \
                 separator)"
            )));
        }
        let (allow, call_timeout_seconds, max_concurrent_calls) = match patch {
            McpServerPatch::Stdio {
                command,
                allow,
                call_timeout_seconds,
                max_concurrent_calls,
                ..
            } => {
                if command.trim().is_empty() {
                    return Err(invalid(format!("mcp server {name:?} has an empty command")));
                }
                (allow, call_timeout_seconds, max_concurrent_calls)
            }
            McpServerPatch::Http {
                url,
                allow,
                call_timeout_seconds,
                max_concurrent_calls,
                ..
            } => {
                let valid_scheme = ["https://", "http://"].into_iter().any(|scheme| {
                    url.len() > scheme.len()
                        && url
                            .get(..scheme.len())
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
                });
                if !valid_scheme {
                    return Err(invalid(format!(
                        "mcp server {name:?} must use an http:// or https:// URL"
                    )));
                }
                (allow, call_timeout_seconds, max_concurrent_calls)
            }
            McpServerPatch::Remove => continue,
        };
        let mut unique = BTreeSet::new();
        for tool in allow {
            if tool.trim().is_empty() {
                return Err(invalid(format!(
                    "mcp server {name:?} allowlists an empty tool name"
                )));
            }
            if !unique.insert(tool) {
                return Err(invalid(format!(
                    "mcp server {name:?} allowlists duplicate tool {tool:?}"
                )));
            }
        }
        if let Some(timeout) = call_timeout_seconds
            && !(1..=MAX_MCP_CALL_TIMEOUT_SECONDS).contains(timeout)
        {
            return Err(invalid(format!(
                "mcp server {name:?} call_timeout_seconds must be between 1 and \
                 {MAX_MCP_CALL_TIMEOUT_SECONDS}"
            )));
        }
        if let Some(bound) = max_concurrent_calls
            && !(1..=MAX_MCP_MAX_CONCURRENT_CALLS).contains(bound)
        {
            return Err(invalid(format!(
                "mcp server {name:?} max_concurrent_calls must be between 1 and \
                 {MAX_MCP_MAX_CONCURRENT_CALLS}"
            )));
        }
    }
    Ok(())
}

fn validate_policy_names(policy: &PolicyPatch, origin: &SourceIdentity) -> Result<(), ConfigError> {
    let invalid = |message: String| ConfigError::Parse {
        origin: origin.clone(),
        message,
    };
    for (field, values) in [
        ("allowed_providers", policy.allowed_providers.as_ref()),
        ("denied_providers", policy.denied_providers.as_ref()),
    ] {
        let Some(values) = values else {
            continue;
        };
        let mut unique = BTreeSet::new();
        for value in values {
            if value.is_empty() {
                return Err(invalid(format!(
                    "policy field {field} contains an empty provider name"
                )));
            }
            if !unique.insert(value) {
                return Err(invalid(format!(
                    "policy field {field} contains duplicate value {value:?}"
                )));
            }
        }
    }
    for (field, entries, tool_shaped) in [
        ("allow_tools", policy.allow_tools.as_deref(), true),
        (
            "allow_shell_prefixes",
            policy.allow_shell_prefixes.as_deref(),
            false,
        ),
    ] {
        let Some(entries) = entries else {
            continue;
        };
        let mut unique = BTreeSet::new();
        for entry in entries {
            let name = entry.name();
            validate_grant_value(name, tool_shaped)
                .map_err(|message| invalid(format!("policy field {field}: {message}")))?;
            if !unique.insert(name) {
                return Err(invalid(format!(
                    "policy field {field} contains duplicate value {name:?}"
                )));
            }
        }
    }
    for (field, values, tool_shaped) in [
        ("deny_tools", policy.deny_tools.as_deref(), true),
        (
            "deny_shell_prefixes",
            policy.deny_shell_prefixes.as_deref(),
            false,
        ),
    ] {
        let Some(values) = values else {
            continue;
        };
        let mut unique = BTreeSet::new();
        for value in values {
            validate_grant_value(value, tool_shaped)
                .map_err(|message| invalid(format!("policy field {field}: {message}")))?;
            if !unique.insert(value) {
                return Err(invalid(format!(
                    "policy field {field} contains duplicate value {value:?}"
                )));
            }
        }
    }
    Ok(())
}

const MAX_TOOL_GRANT_NAME_BYTES: usize = 128;
const MAX_SHELL_PREFIX_GRANT_BYTES: usize = 512;

fn validate_grant_value(value: &str, tool_shaped: bool) -> Result<(), String> {
    if tool_shaped {
        validate_tool_grant_name(value)
    } else {
        validate_shell_prefix_grant(value)
    }
}

/// Tool grants follow tool declaration names: 1-128 bytes of ASCII letters,
/// digits, hyphens, or underscores. Names starting with `mcp__` must complete
/// the `mcp__<server>__<tool>` grammar, with the server segment obeying the
/// MCP server-name rules.
pub(super) fn validate_tool_grant_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_TOOL_GRANT_NAME_BYTES {
        return Err(format!(
            "tool name {name:?} must be 1-{MAX_TOOL_GRANT_NAME_BYTES} bytes"
        ));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!(
            "tool name {name:?} may only use ASCII letters, digits, hyphens, or underscores"
        ));
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        let valid = rest
            .split_once("__")
            .is_some_and(|(server, tool)| valid_mcp_server_name(server) && !tool.is_empty());
        if !valid {
            return Err(format!(
                "MCP tool name {name:?} must take the form mcp__<server>__<tool>"
            ));
        }
    }
    Ok(())
}

/// Shell prefix grants are matched at word granularity, so they must be
/// non-empty, contain no control characters, and carry no surrounding
/// whitespace.
pub(super) fn validate_shell_prefix_grant(prefix: &str) -> Result<(), String> {
    if prefix.trim().is_empty() {
        return Err("shell prefix must not be empty".to_owned());
    }
    if prefix.len() > MAX_SHELL_PREFIX_GRANT_BYTES {
        return Err(format!(
            "shell prefix {prefix:?} must be at most {MAX_SHELL_PREFIX_GRANT_BYTES} bytes"
        ));
    }
    if prefix != prefix.trim() {
        return Err(format!(
            "shell prefix {prefix:?} must not have leading or trailing whitespace"
        ));
    }
    if prefix.chars().any(char::is_control) {
        return Err(format!(
            "shell prefix {prefix:?} must not contain control characters"
        ));
    }
    Ok(())
}

/// Word-granularity overlap between a denied prefix and a granted prefix. A
/// deny removes both narrower grants it covers (`cargo` denies `cargo test`)
/// and broader grants that would cover the denied commands (`cargo test`
/// denied removes a bare `cargo` grant), because a config-layer filter cannot
/// partially subtract a broader grant.
pub(super) fn shell_prefixes_overlap(denied: &str, granted: &str) -> bool {
    word_prefix_covers(denied, granted) || word_prefix_covers(granted, denied)
}

fn word_prefix_covers(prefix: &str, candidate: &str) -> bool {
    candidate == prefix
        || candidate
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// Read-only version-control commands granted by the compiled defaults, per
/// the Version Control section of docs/design/tools.md. Ordinary grants: a
/// later layer can `Remove(...)` any of them and a managed deny filters them.
/// Interrogative subcommands only — nothing here mutates or publishes.
const VCS_READ_ONLY_PRESETS: &[&str] = &[
    "git blame",
    "git diff",
    "git log",
    "git show",
    "git status",
    "jj diff",
    "jj log",
    "jj op log",
    "jj show",
    "jj status",
];

pub(super) struct MergeState {
    organization: Option<String>,
    model: Option<String>,
    worker_model: Option<String>,
    reviewer_model: Option<String>,
    max_output_tokens: u32,
    providers: BTreeMap<String, ProviderConfig>,
    mcp: BTreeMap<String, McpServerConfig>,
    policy: EffectivePolicy,
    provenance: ConfigProvenance,
}

impl MergeState {
    pub(super) fn compiled() -> (Self, SourceReport) {
        let source = SourceIdentity::virtual_source(SourceKind::Compiled, "compiled defaults");
        let providers = BTreeMap::from([
            (
                "anthropic".to_owned(),
                crate::providers::builtin(ProviderKind::Anthropic),
            ),
            (
                "bedrock".to_owned(),
                crate::providers::builtin(ProviderKind::AmazonBedrock),
            ),
            (
                "bedrock-mantle".to_owned(),
                crate::providers::builtin(ProviderKind::AmazonBedrockMantle),
            ),
            (
                "google".to_owned(),
                crate::providers::builtin(ProviderKind::Google),
            ),
            (
                "openai".to_owned(),
                crate::providers::builtin(ProviderKind::OpenAi),
            ),
            (
                "openai-codex".to_owned(),
                crate::providers::builtin(ProviderKind::OpenAiCodex),
            ),
            (
                "xai".to_owned(),
                crate::providers::builtin(ProviderKind::XAi),
            ),
        ]);
        let provenance = ConfigProvenance {
            max_output_tokens: Some(source.clone()),
            providers: providers
                .keys()
                .cloned()
                .map(|name| (name, source.clone()))
                .collect(),
            grant_shell_prefixes: VCS_READ_ONLY_PRESETS
                .iter()
                .map(|prefix| ((*prefix).to_owned(), source.clone()))
                .collect(),
            ..ConfigProvenance::default()
        };
        let report = SourceReport::new(
            source,
            super::SourceStatus::Applied,
            vec![
                ConfigKey::MaxOutputTokens,
                ConfigKey::Providers,
                ConfigKey::Policy,
            ],
        );
        (
            Self {
                organization: None,
                model: None,
                worker_model: None,
                reviewer_model: None,
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                providers,
                mcp: BTreeMap::new(),
                policy: EffectivePolicy {
                    allow_shell_prefixes: VCS_READ_ONLY_PRESETS
                        .iter()
                        .map(|prefix| (*prefix).to_owned())
                        .collect(),
                    ..EffectivePolicy::default()
                },
                provenance,
            },
            report,
        )
    }

    pub(super) fn apply_document(
        &mut self,
        document: &Document,
        source: &SourceIdentity,
        sensitive: bool,
    ) {
        apply_default(
            &document.max_output_tokens,
            &mut self.max_output_tokens,
            DEFAULT_MAX_OUTPUT_TOKENS,
        );
        if document.max_output_tokens.is_present() {
            self.provenance.max_output_tokens = Some(source.clone());
        }
        if !sensitive {
            return;
        }

        if apply_optional_string(&document.organization, &mut self.organization) {
            self.provenance.organization = Some(source.clone());
        }
        if apply_optional_string(&document.model, &mut self.model) {
            self.provenance.model = Some(source.clone());
        }
        if apply_optional_string(&document.worker_model, &mut self.worker_model) {
            self.provenance.worker_model = Some(source.clone());
        }
        if apply_optional_string(&document.reviewer_model, &mut self.reviewer_model) {
            self.provenance.reviewer_model = Some(source.clone());
        }
        self.apply_providers(&document.providers, source);
        self.apply_mcp(&document.mcp);
        if let Some(policy) = &document.policy {
            self.compose_policy(policy, source);
        }
    }

    pub(super) fn apply_runtime(
        &mut self,
        overrides: &RuntimeOverrides,
        source: &SourceIdentity,
    ) -> Vec<ConfigKey> {
        let mut touched = Vec::new();
        if let Some(organization) = &overrides.organization {
            self.organization = Some(organization.clone());
            self.provenance.organization = Some(source.clone());
            touched.push(ConfigKey::Organization);
        }
        if let Some(model) = &overrides.model {
            self.model = Some(model.clone());
            self.provenance.model = Some(source.clone());
            touched.push(ConfigKey::Model);
        }
        if let Some(max_output_tokens) = overrides.max_output_tokens {
            self.max_output_tokens = max_output_tokens;
            self.provenance.max_output_tokens = Some(source.clone());
            touched.push(ConfigKey::MaxOutputTokens);
        }
        touched
    }

    fn apply_providers(
        &mut self,
        patch: &Field<UniqueMap<String, ProviderEntryPatch>>,
        source: &SourceIdentity,
    ) {
        match patch {
            Field::Missing => {}
            Field::Clear => {
                for name in self.providers.keys() {
                    self.provenance
                        .providers
                        .insert(name.clone(), source.clone());
                }
                self.providers.clear();
            }
            Field::Set(patches) => {
                for (name, patch) in &patches.0 {
                    self.apply_provider(name, patch);
                    self.provenance
                        .providers
                        .insert(name.clone(), source.clone());
                }
            }
        }
    }

    fn apply_provider(&mut self, name: &str, patch: &ProviderEntryPatch) {
        match patch {
            ProviderEntryPatch::Remove => {
                self.providers.remove(name);
            }
            ProviderEntryPatch::OpenAi { api_key, models } => {
                let provider = self.provider_for_patch(name, ProviderKind::OpenAi);
                let current = api_key_slot(provider, ProviderKind::OpenAi);
                apply_optional(api_key, current);
                apply_models(models, provider.models_mut());
            }
            ProviderEntryPatch::OpenAiCodex { profile, models } => {
                let provider = self.provider_for_patch(name, ProviderKind::OpenAiCodex);
                let Some(ProviderAccess::Http(access)) = provider.access_mut().as_mut() else {
                    unreachable!("OpenAI Codex preset is HTTP")
                };
                let HttpCredential::OpenAiCodex { profile: current } = &mut access.auth else {
                    unreachable!("OpenAI Codex preset uses Codex credentials")
                };
                apply_optional_string(profile, current);
                apply_models(models, provider.models_mut());
            }
            ProviderEntryPatch::Anthropic { api_key, models } => {
                let provider = self.provider_for_patch(name, ProviderKind::Anthropic);
                let current = api_key_slot(provider, ProviderKind::Anthropic);
                apply_optional(api_key, current);
                apply_models(models, provider.models_mut());
            }
            ProviderEntryPatch::Google { api_key, models } => {
                let provider = self.provider_for_patch(name, ProviderKind::Google);
                let current = api_key_slot(provider, ProviderKind::Google);
                apply_optional(api_key, current);
                apply_models(models, provider.models_mut());
            }
            ProviderEntryPatch::XAi {
                api_key,
                profile,
                models,
            } => {
                let provider = self.provider_for_patch(name, ProviderKind::XAi);
                let Some(ProviderAccess::Http(access)) = provider.access_mut().as_mut() else {
                    unreachable!("xAI preset is HTTP")
                };
                let HttpCredential::XAi {
                    api_key: current_key,
                    profile: current_profile,
                } = &mut access.auth
                else {
                    unreachable!("xAI preset uses xAI credentials")
                };
                apply_optional(api_key, current_key);
                apply_optional_string(profile, current_profile);
                apply_models(models, provider.models_mut());
            }
            ProviderEntryPatch::LiteLlm { connection, models } => {
                let provider = self.provider_for_patch(name, ProviderKind::LiteLlm);
                apply_connection(connection, provider);
                apply_models(models, provider.models_mut());
            }
            ProviderEntryPatch::AmazonBedrock {
                region,
                auth,
                models,
            } => {
                let provider = self.provider_for_patch(name, ProviderKind::AmazonBedrock);
                let Some(ProviderAccess::AmazonBedrock {
                    region: current_region,
                    auth: current_auth,
                }) = provider.access_mut().as_mut()
                else {
                    unreachable!("Bedrock preset uses Bedrock access")
                };
                apply_optional_string(region, current_region);
                apply_default(auth, current_auth, BedrockAuth::Aws(AwsAuth::DefaultChain));
                apply_models(models, provider.models_mut());
            }
            ProviderEntryPatch::AmazonBedrockMantle {
                region,
                api,
                auth,
                models,
            } => {
                let provider = self.provider_for_patch(name, ProviderKind::AmazonBedrockMantle);
                let Some(ProviderAccess::AmazonBedrockMantle {
                    region: current_region,
                    api: current_api,
                    auth: current_auth,
                }) = provider.access_mut().as_mut()
                else {
                    unreachable!("Mantle preset uses Mantle access")
                };
                apply_optional_string(region, current_region);
                apply_default(api, current_api, ProviderApi::OpenAiResponses);
                apply_default(auth, current_auth, BedrockAuth::Aws(AwsAuth::DefaultChain));
                apply_models(models, provider.models_mut());
            }
            ProviderEntryPatch::Custom { connection, models } => {
                let provider = self.provider_for_patch(name, ProviderKind::Custom);
                apply_connection(connection, provider);
                apply_models(models, provider.models_mut());
            }
        }
    }

    fn apply_mcp(&mut self, patch: &Field<UniqueMap<String, McpServerPatch>>) {
        match patch {
            Field::Missing => {}
            Field::Clear => self.mcp.clear(),
            Field::Set(patches) => {
                for (name, patch) in &patches.0 {
                    match patch {
                        McpServerPatch::Remove => {
                            self.mcp.remove(name);
                        }
                        McpServerPatch::Stdio {
                            command,
                            args,
                            env,
                            eager,
                            allow,
                            call_timeout_seconds,
                            max_concurrent_calls,
                        } => {
                            self.mcp.insert(
                                name.clone(),
                                McpServerConfig::new(
                                    McpTransport::Stdio {
                                        command: command.clone(),
                                        args: args.clone(),
                                        env: env.clone(),
                                    },
                                    *eager,
                                    allow.clone(),
                                    call_timeout_seconds
                                        .unwrap_or(DEFAULT_MCP_CALL_TIMEOUT_SECONDS),
                                    max_concurrent_calls
                                        .unwrap_or(DEFAULT_MCP_MAX_CONCURRENT_CALLS),
                                ),
                            );
                        }
                        McpServerPatch::Http {
                            url,
                            bearer,
                            eager,
                            allow,
                            call_timeout_seconds,
                            max_concurrent_calls,
                        } => {
                            self.mcp.insert(
                                name.clone(),
                                McpServerConfig::new(
                                    McpTransport::Http {
                                        url: url.clone(),
                                        bearer: bearer.clone(),
                                    },
                                    *eager,
                                    allow.clone(),
                                    call_timeout_seconds
                                        .unwrap_or(DEFAULT_MCP_CALL_TIMEOUT_SECONDS),
                                    max_concurrent_calls
                                        .unwrap_or(DEFAULT_MCP_MAX_CONCURRENT_CALLS),
                                ),
                            );
                        }
                    }
                }
            }
        }
    }

    fn provider_for_patch(&mut self, name: &str, kind: ProviderKind) -> &mut ProviderConfig {
        let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
            let mut provider = crate::providers::builtin(kind);
            provider.models_mut().clear();
            provider
        });
        if provider.kind() != kind {
            let mut replacement = crate::providers::builtin(kind);
            replacement.models_mut().clear();
            *provider = replacement;
        }
        provider
    }

    fn compose_policy(&mut self, patch: &PolicyPatch, source: &SourceIdentity) {
        if let Some(incoming) = &patch.allowed_providers {
            let incoming: BTreeSet<_> = incoming.iter().cloned().collect();
            let combined = match &self.policy.allowed_providers {
                Some(current) => current
                    .iter()
                    .filter(|name| incoming.contains(*name))
                    .cloned()
                    .collect(),
                None => incoming.into_iter().collect(),
            };
            self.policy.allowed_providers = Some(combined);
        }
        if let Some(incoming) = &patch.denied_providers {
            let mut combined: BTreeSet<_> = self.policy.denied_providers.iter().cloned().collect();
            combined.extend(incoming.iter().cloned());
            self.policy.denied_providers = combined.into_iter().collect();
        }
        if let Some(incoming) = patch.max_output_tokens {
            self.policy.max_output_tokens = Some(
                self.policy
                    .max_output_tokens
                    .map_or(incoming, |current| current.min(incoming)),
            );
        }
        if let Some(incoming) = patch.require_https {
            self.policy.require_https |= incoming;
        }
        if let Some(incoming) = patch.allow_custom_providers {
            self.policy.allow_custom_providers &= incoming;
        }
        if let Some(incoming) = patch.allow_literal_secrets {
            self.policy.allow_literal_secrets &= incoming;
        }
        apply_grant_entries(
            patch.allow_tools.as_deref(),
            &mut self.policy.allow_tools,
            &mut self.provenance.grant_tools,
            source,
        );
        apply_grant_entries(
            patch.allow_shell_prefixes.as_deref(),
            &mut self.policy.allow_shell_prefixes,
            &mut self.provenance.grant_shell_prefixes,
            source,
        );
        // Denies are monotonic across layers, like denied_providers.
        if let Some(incoming) = &patch.deny_tools {
            let mut combined: BTreeSet<_> = self.policy.deny_tools.iter().cloned().collect();
            combined.extend(incoming.iter().cloned());
            self.policy.deny_tools = combined.into_iter().collect();
        }
        if let Some(incoming) = &patch.deny_shell_prefixes {
            let mut combined: BTreeSet<_> =
                self.policy.deny_shell_prefixes.iter().cloned().collect();
            combined.extend(incoming.iter().cloned());
            self.policy.deny_shell_prefixes = combined.into_iter().collect();
        }
    }

    pub(super) fn finish(self, reports: Vec<SourceReport>) -> Result<ConfigSnapshot, ConfigError> {
        let model = ModelRoute::parse(self.model.ok_or(ConfigError::ModelRequired)?)?;
        let worker_model = self.worker_model.map(ModelRoute::parse).transpose()?;
        let reviewer_model = self.reviewer_model.map(ModelRoute::parse).transpose()?;
        for route in std::iter::once(&model)
            .chain(worker_model.as_ref())
            .chain(reviewer_model.as_ref())
        {
            if !self.providers.contains_key(route.provider()) {
                return Err(ConfigError::UnknownProvider(route.provider().to_owned()));
            }
            enforce_policy(&self.policy, route, self.max_output_tokens, &self.providers)?;
        }
        let grants = resolve_policy_grants(&self.policy, &self.mcp);
        Ok(ConfigSnapshot {
            organization: self.organization,
            model,
            worker_model,
            reviewer_model,
            max_output_tokens: self.max_output_tokens,
            providers: self.providers,
            mcp: self.mcp,
            policy: self.policy,
            grants,
            reports,
            provenance: self.provenance,
        })
    }
}

fn apply_grant_entries(
    entries: Option<&[GrantEntry]>,
    current: &mut Vec<String>,
    provenance: &mut BTreeMap<String, SourceIdentity>,
    source: &SourceIdentity,
) {
    let Some(entries) = entries else {
        return;
    };
    let mut combined: BTreeSet<String> = current.drain(..).collect();
    for entry in entries {
        match entry {
            GrantEntry::Allow(name) => {
                combined.insert(name.clone());
                provenance.insert(name.clone(), source.clone());
            }
            GrantEntry::Remove(GrantRemoval::Remove(name)) => {
                combined.remove(name);
                provenance.remove(name);
            }
        }
    }
    *current = combined.into_iter().collect();
}

/// Resolves the effective grant set: declared tool and shell-prefix grants
/// plus every per-MCP-server allowlist entry folded in as an exact
/// `mcp__<server>__<tool>` name, minus everything the managed deny lists
/// filter out.
fn resolve_policy_grants(
    policy: &EffectivePolicy,
    mcp: &BTreeMap<String, McpServerConfig>,
) -> PolicyGrants {
    let mut tools: BTreeSet<String> = policy.allow_tools.iter().cloned().collect();
    for (server, config) in mcp {
        for tool in config.allow() {
            tools.insert(format!("mcp__{server}__{tool}"));
        }
    }
    let tools = tools
        .into_iter()
        .filter(|name| !policy.deny_tools.iter().any(|denied| denied == name))
        .collect();
    let shell_prefixes = policy
        .allow_shell_prefixes
        .iter()
        .filter(|granted| {
            !policy
                .deny_shell_prefixes
                .iter()
                .any(|denied| shell_prefixes_overlap(denied, granted))
        })
        .cloned()
        .collect();
    PolicyGrants::new(tools, shell_prefixes)
}

fn apply_optional<T: Clone>(field: &Field<T>, current: &mut Option<T>) -> bool {
    match field {
        Field::Missing => false,
        Field::Set(value) => {
            *current = Some(value.clone());
            true
        }
        Field::Clear => {
            *current = None;
            true
        }
    }
}

fn apply_optional_string(field: &StringField, current: &mut Option<String>) -> bool {
    match field {
        StringField::Missing => false,
        StringField::Set(value) => {
            *current = Some(value.clone());
            true
        }
        StringField::Clear => {
            *current = None;
            true
        }
    }
}

fn apply_default<T: Clone>(field: &Field<T>, current: &mut T, default: T) -> bool {
    match field {
        Field::Missing => false,
        Field::Set(value) => {
            *current = value.clone();
            true
        }
        Field::Clear => {
            *current = default;
            true
        }
    }
}

fn api_key_slot(provider: &mut ProviderConfig, kind: ProviderKind) -> &mut Option<SecretRef> {
    let Some(ProviderAccess::Http(access)) = provider.access_mut().as_mut() else {
        unreachable!("{kind:?} preset is HTTP")
    };
    let HttpCredential::ApiKey { explicit, .. } = &mut access.auth else {
        unreachable!("{kind:?} preset uses API-key credentials")
    };
    explicit
}

fn apply_connection(field: &Field<Connection>, provider: &mut ProviderConfig) {
    match field {
        Field::Missing => {}
        Field::Clear => *provider.access_mut() = None,
        Field::Set(connection) => {
            *provider.access_mut() = Some(ProviderAccess::Http(HttpAccess::configured(connection)));
        }
    }
}

fn apply_models(
    patch: &Field<UniqueMap<String, ModelEntryPatch>>,
    models: &mut BTreeMap<String, ModelMetadata>,
) {
    match patch {
        Field::Missing => {}
        Field::Clear => models.clear(),
        Field::Set(patches) => {
            for (name, patch) in &patches.0 {
                match patch {
                    ModelEntryPatch::Remove(RemoveMarker::Remove) => {
                        models.remove(name);
                    }
                    ModelEntryPatch::Set(patch) => {
                        apply_model_patch(models.entry(name.clone()).or_default(), patch);
                    }
                }
            }
        }
    }
}

fn apply_model_patch(model: &mut ModelMetadata, patch: &ModelPatch) {
    apply_optional_string(&patch.name, &mut model.name);
    apply_optional(&patch.api, &mut model.api);
    apply_default(&patch.reasoning, &mut model.reasoning, false);
    apply_default(&patch.input, &mut model.input, Vec::new());
    apply_optional(&patch.context_window, &mut model.context_window);
    apply_optional(&patch.max_output_tokens, &mut model.max_output_tokens);
    apply_optional(&patch.pricing, &mut model.pricing);
}

fn enforce_policy(
    policy: &EffectivePolicy,
    model: &ModelRoute,
    max_output_tokens: u32,
    providers: &BTreeMap<String, ProviderConfig>,
) -> Result<(), ConfigError> {
    if let Some(allowed) = &policy.allowed_providers
        && !allowed.iter().any(|provider| provider == model.provider())
    {
        return Err(policy_violation(
            "allowed_providers",
            format!("provider {:?} is not allowed", model.provider()),
        ));
    }
    if policy
        .denied_providers
        .iter()
        .any(|provider| provider == model.provider())
    {
        return Err(policy_violation(
            "denied_providers",
            format!("provider {:?} is denied", model.provider()),
        ));
    }
    if let Some(limit) = policy.max_output_tokens
        && max_output_tokens > limit
    {
        return Err(policy_violation(
            "max_output_tokens",
            format!("configured value {max_output_tokens} exceeds {limit}"),
        ));
    }
    if !policy.allow_custom_providers
        && providers.values().any(ProviderConfig::uses_custom_endpoint)
    {
        return Err(policy_violation(
            "allow_custom_providers",
            "a custom or LiteLLM provider is configured".to_owned(),
        ));
    }
    if !policy.allow_literal_secrets
        && providers
            .values()
            .any(ProviderConfig::contains_literal_secret)
    {
        return Err(policy_violation(
            "allow_literal_secrets",
            "a literal secret or static header value is configured".to_owned(),
        ));
    }
    if policy.require_https {
        for (name, provider) in providers {
            if provider.uses_custom_endpoint()
                && let Some(ProviderAccess::Http(access)) = provider.access()
                && !has_https_scheme(access.endpoint())
            {
                return Err(policy_violation(
                    "require_https",
                    format!("provider {name:?} has a non-HTTPS base URL"),
                ));
            }
        }
    }
    Ok(())
}

fn has_https_scheme(value: &str) -> bool {
    value
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

fn policy_violation(rule: &'static str, message: String) -> ConfigError {
    ConfigError::PolicyViolation { rule, message }
}
