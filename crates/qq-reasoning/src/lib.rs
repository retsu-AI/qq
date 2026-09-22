//! Provider-independent types for displayable model reasoning.
//!
//! Reasoning is presentation telemetry, not assistant transcript content.
//! Only provider-generated summaries and thinking text explicitly exposed by
//! an API belong here. Encrypted payloads, signatures, and opaque continuation
//! state must remain private to provider adapters.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// The disclosure level of reasoning text made available to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningKind {
    /// A provider-generated summary of otherwise hidden reasoning.
    Summary,
    /// Thinking text explicitly exposed by the provider API.
    ExposedThinking,
}

/// Provider-neutral effort control for reasoning models.
///
/// These are the values accepted by the current OpenAI Responses and Chat
/// Completions contracts. Model-specific support remains a runtime/provider
/// capability question; adapters must reject values they cannot encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    /// Every effort value, lowest to highest.
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

/// One lifecycle event for a displayable reasoning block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningEvent {
    Started { kind: ReasoningKind },
    Delta { kind: ReasoningKind, text: String },
    Completed { kind: ReasoningKind },
}

impl ReasoningEvent {
    #[must_use]
    pub const fn kind(&self) -> ReasoningKind {
        match self {
            Self::Started { kind } | Self::Delta { kind, .. } | Self::Completed { kind } => *kind,
        }
    }
}
