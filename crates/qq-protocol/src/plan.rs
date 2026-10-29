use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::ContentHash;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentPlanDigest(ContentHash);

impl AgentPlanDigest {
    #[must_use]
    pub const fn from_hash(hash: ContentHash) -> Self {
        Self(hash)
    }

    #[must_use]
    pub const fn as_hash(&self) -> &ContentHash {
        &self.0
    }
}

impl fmt::Display for AgentPlanDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct CredentialEpoch(u64);

impl CredentialEpoch {
    pub const NONE: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for CredentialEpoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

pub const MAX_PROFILE_ID_BYTES: usize = 64;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentProfileId(String);

impl AgentProfileId {
    pub const DEFAULT: &'static str = "default";

    pub fn new(value: impl Into<String>) -> Result<Self, AgentProfileIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(AgentProfileIdError::Empty);
        }
        if value.len() > MAX_PROFILE_ID_BYTES {
            return Err(AgentProfileIdError::TooLong);
        }
        let bytes = value.as_bytes();
        if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
            return Err(AgentProfileIdError::InvalidStart);
        }
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        {
            return Err(AgentProfileIdError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT
    }
}

impl Default for AgentProfileId {
    fn default() -> Self {
        Self(Self::DEFAULT.to_owned())
    }
}

impl FromStr for AgentProfileId {
    type Err = AgentProfileIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl fmt::Display for AgentProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for AgentProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("AgentProfileId")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for AgentProfileId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AgentProfileIdError {
    #[error("agent profile id must not be empty")]
    Empty,
    #[error("agent profile id exceeds {MAX_PROFILE_ID_BYTES} bytes")]
    TooLong,
    #[error("agent profile id must start with a lowercase letter or digit")]
    InvalidStart,
    #[error("agent profile id may contain only lowercase letters, digits, and hyphens")]
    InvalidCharacter,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPlanIdentity {
    pub profile: AgentProfileId,
    pub descriptor_version: u16,
    pub digest: AgentPlanDigest,
    pub credential_epoch: CredentialEpoch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_digest_serializes_as_the_hex_hash() {
        let digest = AgentPlanDigest::from_hash(ContentHash::from_bytes([0xab; 32]));
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
        let decoded: AgentPlanDigest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, digest);
        assert_eq!(digest.to_string(), "ab".repeat(32));
    }

    #[test]
    fn credential_epoch_is_a_transparent_ordered_integer() {
        let epoch = CredentialEpoch::new(7);
        assert_eq!(serde_json::to_string(&epoch).unwrap(), "7");
        assert_eq!(
            serde_json::from_str::<CredentialEpoch>("8").unwrap(),
            CredentialEpoch::new(8)
        );
        assert!(CredentialEpoch::NONE < epoch);
        assert_eq!(CredentialEpoch::default(), CredentialEpoch::NONE);
        assert_eq!(epoch.get(), 7);
    }
    #[test]
    fn profile_ids_are_bounded_lowercase_slugs() {
        assert_eq!(AgentProfileId::default().as_str(), "default");
        assert!(AgentProfileId::default().is_default());
        assert_eq!(
            AgentProfileId::new("review-2").unwrap().to_string(),
            "review-2"
        );
        assert_eq!(AgentProfileId::new(""), Err(AgentProfileIdError::Empty));
        assert_eq!(
            AgentProfileId::new("a".repeat(MAX_PROFILE_ID_BYTES + 1)),
            Err(AgentProfileIdError::TooLong)
        );
        assert_eq!(
            AgentProfileId::new("-x"),
            Err(AgentProfileIdError::InvalidStart)
        );
        assert_eq!(
            AgentProfileId::new("Review"),
            Err(AgentProfileIdError::InvalidStart)
        );
        assert_eq!(
            AgentProfileId::new("rEview"),
            Err(AgentProfileIdError::InvalidCharacter)
        );
        assert_eq!(
            AgentProfileId::new("a_b"),
            Err(AgentProfileIdError::InvalidCharacter)
        );
        assert!(serde_json::from_str::<AgentProfileId>("\"Bad\"").is_err());
        assert_eq!(
            serde_json::to_string(&AgentProfileId::new("fast").unwrap()).unwrap(),
            "\"fast\""
        );
        assert_eq!(
            format!("{:?}", AgentProfileId::new("fast").unwrap()),
            "AgentProfileId(\"fast\")"
        );
    }

    #[test]
    fn run_plan_identity_round_trips() {
        let identity = RunPlanIdentity {
            profile: AgentProfileId::new("fast").unwrap(),
            descriptor_version: 2,
            digest: AgentPlanDigest::from_hash(ContentHash::from_bytes([0x01; 32])),
            credential_epoch: CredentialEpoch::new(3),
        };
        let json = serde_json::to_string(&identity).unwrap();
        assert_eq!(
            json,
            format!(
                "{{\"profile\":\"fast\",\"descriptor_version\":2,\"digest\":\"{}\",\"credential_epoch\":3}}",
                "01".repeat(32)
            )
        );
        assert_eq!(
            serde_json::from_str::<RunPlanIdentity>(&json).unwrap(),
            identity
        );
    }
}
