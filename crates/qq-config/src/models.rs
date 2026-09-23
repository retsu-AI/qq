//! Bundled model catalog generated from a pinned models.dev projection.

use std::collections::BTreeMap;

use crate::{ModelMetadata, ModelPricing, ModelPricingTier, ProviderApi};
use qq_provider::ReasoningEffort;

const PROVENANCE: &str = "models.dev/api.json@2026-09-21";
const OPENAI_API: u16 = 1 << 0;
const OPENAI_CODEX: u16 = 1 << 1;
const ANTHROPIC_API: u16 = 1 << 2;
const GOOGLE_API: u16 = 1 << 3;
const BEDROCK_RUNTIME: u16 = 1 << 4;
const BEDROCK_MANTLE: u16 = 1 << 5;
const XAI_API: u16 = 1 << 6;

#[derive(Clone, Copy)]
pub(crate) enum BuiltinCatalog {
    OpenAiApi,
    OpenAiCodex,
    AnthropicApi,
    GoogleApi,
    BedrockRuntime,
    BedrockMantle,
    XAiApi,
}

impl BuiltinCatalog {
    const fn mask(self) -> u16 {
        match self {
            Self::OpenAiApi => OPENAI_API,
            Self::OpenAiCodex => OPENAI_CODEX,
            Self::AnthropicApi => ANTHROPIC_API,
            Self::GoogleApi => GOOGLE_API,
            Self::BedrockRuntime => BEDROCK_RUNTIME,
            Self::BedrockMantle => BEDROCK_MANTLE,
            Self::XAiApi => XAI_API,
        }
    }
}

struct ModelDefinition {
    catalogs: u16,
    wire_id: &'static str,
    canonical_id: &'static str,
    name: &'static str,
    reasoning: bool,
    /// Effort values the provider documents for this model. Empty means the
    /// catalog does not advertise a set: either the adapter never transmits
    /// effort (Anthropic, Google, Bedrock Claude) or the provider has not
    /// published one. Only the OpenAI-shaped adapters send `reasoning_effort`.
    efforts: &'static [ReasoningEffort],
    context_tokens: u32,
    output_tokens: u32,
    pricing: Option<PricingDefinition>,
    api: Option<ProviderApi>,
}

/// The documented OpenAI reasoning ladder. `minimal` through `xhigh`; `none`
/// is a request-level opt-out rather than a model capability, so it is not
/// advertised here.
const OPENAI_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Xhigh,
];

/// xAI's Responses/Chat deployments accept the `low`/`high` pair.
const XAI_EFFORTS: &[ReasoningEffort] = &[ReasoningEffort::Low, ReasoningEffort::High];

#[derive(Clone, Copy)]
struct PricingDefinition {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: Option<u64>,
    context_tier: Option<PricingTierDefinition>,
}

#[derive(Clone, Copy)]
struct PricingTierDefinition {
    above_input_tokens: u64,
    input: u64,
    output: u64,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

macro_rules! model {
    (
        catalogs: $catalogs:expr,
        wire: $wire:literal,
        canonical: $canonical:literal,
        name: $name:literal,
        reasoning: $reasoning:literal,
        $(efforts: $efforts:expr,)?
        limits: $context:literal / $output:literal,
        pricing: $pricing:expr
        $(, api: $api:expr)?
    ) => {
        ModelDefinition {
            catalogs: $catalogs,
            wire_id: $wire,
            canonical_id: $canonical,
            name: $name,
            reasoning: $reasoning,
            efforts: model!(@efforts $($efforts)?),
            context_tokens: $context,
            output_tokens: $output,
            pricing: $pricing,
            api: model!(@api $($api)?),
        }
    };
    (@api $api:expr) => { Some($api) };
    (@api) => { None };
    (@efforts $efforts:expr) => { $efforts };
    (@efforts) => { &[] };
}

const fn metered(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> Option<PricingDefinition> {
    Some(PricingDefinition {
        input,
        output,
        cache_read,
        cache_write: if cache_write == 0 {
            None
        } else {
            Some(cache_write)
        },
        context_tier: None,
    })
}

const fn tiered(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    tier: PricingTierDefinition,
) -> Option<PricingDefinition> {
    Some(PricingDefinition {
        input,
        output,
        cache_read,
        cache_write: if cache_write == 0 {
            None
        } else {
            Some(cache_write)
        },
        context_tier: Some(tier),
    })
}

// GPT-6 excludes the legacy `minimal` effort. `max` is not yet represented
// by QQ's shared effort vocabulary.
const GPT6_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Xhigh,
];

const MODELS: &[ModelDefinition] = &[
    model! { catalogs: OPENAI_API, wire: "gpt-6-sol", canonical: "openai/gpt-6-sol", name: "GPT-6 Sol", reasoning: true, efforts: GPT6_EFFORTS, limits: 1_050_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_API, wire: "gpt-6-luna", canonical: "openai/gpt-6-luna", name: "GPT-6 Luna", reasoning: true, efforts: GPT6_EFFORTS, limits: 1_050_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-6-sol", canonical: "openai/gpt-6-sol", name: "GPT-6 Sol", reasoning: true, efforts: GPT6_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-6-luna", canonical: "openai/gpt-6-luna", name: "GPT-6 Luna", reasoning: true, efforts: GPT6_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! {
        catalogs: ANTHROPIC_API,
        wire: "claude-sonnet-5",
        canonical: "anthropic/claude-sonnet-5",
        name: "Claude Sonnet 5",
        reasoning: true,
        limits: 1_000_000 / 128_000,
        pricing: metered(2_000, 10_000, 200, 2_500)
    },
    model! {
        catalogs: ANTHROPIC_API,
        wire: "claude-opus-4-8",
        canonical: "anthropic/claude-opus-4-8",
        name: "Claude Opus 4.8",
        reasoning: true,
        limits: 1_000_000 / 128_000,
        pricing: metered(5_000, 25_000, 500, 6_250)
    },
    model! {
        catalogs: ANTHROPIC_API,
        wire: "claude-opus-4-7",
        canonical: "anthropic/claude-opus-4-7",
        name: "Claude Opus 4.7",
        reasoning: true,
        limits: 1_000_000 / 128_000,
        pricing: metered(5_000, 25_000, 500, 6_250)
    },
    model! {
        catalogs: ANTHROPIC_API,
        wire: "claude-opus-4-6",
        canonical: "anthropic/claude-opus-4-6",
        name: "Claude Opus 4.6",
        reasoning: true,
        limits: 1_000_000 / 128_000,
        pricing: metered(5_000, 25_000, 500, 6_250)
    },
    model! {
        catalogs: ANTHROPIC_API,
        wire: "claude-sonnet-4-6",
        canonical: "anthropic/claude-sonnet-4-6",
        name: "Claude Sonnet 4.6",
        reasoning: true,
        limits: 1_000_000 / 128_000,
        pricing: metered(3_000, 15_000, 300, 3_750)
    },
    model! {
        catalogs: ANTHROPIC_API,
        wire: "claude-haiku-4-5",
        canonical: "anthropic/claude-haiku-4-5",
        name: "Claude Haiku 4.5",
        reasoning: true,
        limits: 200_000 / 64_000,
        pricing: metered(1_000, 5_000, 100, 1_250)
    },
    model! {
        catalogs: ANTHROPIC_API,
        wire: "claude-sonnet-4-5",
        canonical: "anthropic/claude-sonnet-4-5",
        name: "Claude Sonnet 4.5",
        reasoning: true,
        limits: 1_000_000 / 64_000,
        pricing: metered(3_000, 15_000, 300, 3_750)
    },
    model! {
        catalogs: ANTHROPIC_API,
        wire: "claude-opus-4-5",
        canonical: "anthropic/claude-opus-4-5",
        name: "Claude Opus 4.5",
        reasoning: true,
        limits: 200_000 / 64_000,
        pricing: metered(5_000, 25_000, 500, 6_250)
    },
    model! {
        catalogs: OPENAI_API,
        wire: "gpt-6-astra",
        canonical: "openai/gpt-6-astra",
        name: "GPT-6 Astra",
        reasoning: true,
        efforts: OPENAI_EFFORTS,
        limits: 1_050_000 / 128_000,
        pricing: tiered(10_000, 50_000, 1_000, 12_500, PricingTierDefinition {
            above_input_tokens: 272_000, input: 20_000, output: 75_000,
            cache_read: Some(2_000), cache_write: Some(25_000),
        })
    },
    model! {
        catalogs: OPENAI_API,
        wire: "gpt-5.6",
        canonical: "openai/gpt-5.6",
        name: "GPT-5.6",
        reasoning: true,
        efforts: OPENAI_EFFORTS,
        limits: 1_050_000 / 128_000,
        pricing: tiered(5_000, 30_000, 500, 6_250, PricingTierDefinition {
            above_input_tokens: 272_000, input: 10_000, output: 45_000,
            cache_read: Some(1_000), cache_write: Some(12_500),
        })
    },
    model! {
        catalogs: OPENAI_API,
        wire: "gpt-5.6-sol",
        canonical: "openai/gpt-5.6-sol",
        name: "GPT-5.6 Sol",
        reasoning: true,
        efforts: OPENAI_EFFORTS,
        limits: 1_050_000 / 128_000,
        pricing: tiered(5_000, 30_000, 500, 6_250, PricingTierDefinition {
            above_input_tokens: 272_000, input: 10_000, output: 45_000,
            cache_read: Some(1_000), cache_write: Some(12_500),
        })
    },
    model! {
        catalogs: OPENAI_API,
        wire: "gpt-5.6-luna",
        canonical: "openai/gpt-5.6-luna",
        name: "GPT-5.6 Luna",
        reasoning: true,
        efforts: OPENAI_EFFORTS,
        limits: 1_050_000 / 128_000,
        pricing: tiered(1_000, 6_000, 100, 1_250, PricingTierDefinition {
            above_input_tokens: 272_000, input: 2_000, output: 9_000,
            cache_read: Some(200), cache_write: Some(2_500),
        })
    },
    model! {
        catalogs: OPENAI_API,
        wire: "gpt-5.4",
        canonical: "openai/gpt-5.4",
        name: "GPT-5.4",
        reasoning: true,
        efforts: OPENAI_EFFORTS,
        limits: 1_050_000 / 128_000,
        pricing: tiered(2_500, 15_000, 250, 0, PricingTierDefinition {
            above_input_tokens: 272_000, input: 5_000, output: 22_500,
            cache_read: Some(500), cache_write: None,
        })
    },
    model! { catalogs: OPENAI_API, wire: "gpt-5.4-mini", canonical: "openai/gpt-5.4-mini", name: "GPT-5.4 mini", reasoning: true, efforts: OPENAI_EFFORTS, limits: 400_000 / 128_000, pricing: metered(750, 4_500, 75, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-5.4-nano", canonical: "openai/gpt-5.4-nano", name: "GPT-5.4 nano", reasoning: true, efforts: OPENAI_EFFORTS, limits: 400_000 / 128_000, pricing: metered(200, 1_250, 20, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-5.2", canonical: "openai/gpt-5.2", name: "GPT-5.2", reasoning: true, efforts: OPENAI_EFFORTS, limits: 400_000 / 128_000, pricing: metered(1_750, 14_000, 175, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-5-mini", canonical: "openai/gpt-5-mini", name: "GPT-5 Mini", reasoning: true, efforts: OPENAI_EFFORTS, limits: 400_000 / 128_000, pricing: metered(250, 2_000, 25, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-5-nano", canonical: "openai/gpt-5-nano", name: "GPT-5 Nano", reasoning: true, efforts: OPENAI_EFFORTS, limits: 400_000 / 128_000, pricing: metered(50, 400, 5, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-4.1", canonical: "openai/gpt-4.1", name: "GPT-4.1", reasoning: false, limits: 1_047_576 / 32_768, pricing: metered(2_000, 8_000, 500, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-4.1-mini", canonical: "openai/gpt-4.1-mini", name: "GPT-4.1 mini", reasoning: false, limits: 1_047_576 / 32_768, pricing: metered(400, 1_600, 100, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-4.1-nano", canonical: "openai/gpt-4.1-nano", name: "GPT-4.1 nano", reasoning: false, limits: 1_047_576 / 32_768, pricing: metered(100, 400, 25, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-4o", canonical: "openai/gpt-4o", name: "GPT-4o", reasoning: false, limits: 128_000 / 16_384, pricing: metered(2_500, 10_000, 1_250, 0) },
    model! { catalogs: OPENAI_API, wire: "gpt-4o-mini", canonical: "openai/gpt-4o-mini", name: "GPT-4o mini", reasoning: false, limits: 128_000 / 16_384, pricing: metered(150, 600, 75, 0) },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-6-astra", canonical: "openai/gpt-6-astra", name: "GPT-6 Astra", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-5.6-sol", canonical: "openai/gpt-5.6-sol", name: "GPT-5.6 Sol", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-5.6-terra", canonical: "openai/gpt-5.6-terra", name: "GPT-5.6 Terra", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-5.6-luna", canonical: "openai/gpt-5.6-luna", name: "GPT-5.6 Luna", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-5.5", canonical: "openai/gpt-5.5", name: "GPT-5.5", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-5.4", canonical: "openai/gpt-5.4", name: "GPT-5.4", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-5.4-mini", canonical: "openai/gpt-5.4-mini", name: "GPT-5.4 Mini", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: None },
    model! { catalogs: OPENAI_CODEX, wire: "gpt-5.3-codex-spark", canonical: "openai/gpt-5.3-codex-spark", name: "GPT-5.3 Codex Spark", reasoning: true, efforts: OPENAI_EFFORTS, limits: 128_000 / 32_000, pricing: None },
    model! { catalogs: GOOGLE_API, wire: "gemini-3.6-flash", canonical: "google/gemini-3.6-flash", name: "Gemini 3.6 Flash", reasoning: true, limits: 1_048_576 / 65_536, pricing: metered(1_500, 7_500, 150, 0) },
    model! { catalogs: GOOGLE_API, wire: "gemini-3.5-flash", canonical: "google/gemini-3.5-flash", name: "Gemini 3.5 Flash", reasoning: true, limits: 1_048_576 / 65_536, pricing: metered(1_500, 9_000, 150, 0) },
    model! { catalogs: GOOGLE_API, wire: "gemini-3.5-flash-lite", canonical: "google/gemini-3.5-flash-lite", name: "Gemini 3.5 Flash Lite", reasoning: true, limits: 1_048_576 / 65_536, pricing: metered(300, 2_500, 30, 0) },
    model! {
        catalogs: GOOGLE_API,
        wire: "gemini-3.1-pro-preview",
        canonical: "google/gemini-3.1-pro-preview",
        name: "Gemini 3.1 Pro Preview",
        reasoning: true,
        limits: 1_048_576 / 65_536,
        pricing: tiered(2_000, 12_000, 200, 0, PricingTierDefinition {
            above_input_tokens: 200_000, input: 4_000, output: 18_000,
            cache_read: Some(400), cache_write: None,
        })
    },
    model! { catalogs: GOOGLE_API, wire: "gemini-3.1-flash-lite", canonical: "google/gemini-3.1-flash-lite", name: "Gemini 3.1 Flash Lite", reasoning: true, limits: 1_048_576 / 65_536, pricing: metered(250, 1_500, 25, 0) },
    model! { catalogs: GOOGLE_API, wire: "gemini-3-flash-preview", canonical: "google/gemini-3-flash-preview", name: "Gemini 3 Flash Preview", reasoning: true, limits: 1_048_576 / 65_536, pricing: metered(500, 3_000, 50, 0) },
    model! {
        catalogs: GOOGLE_API,
        wire: "gemini-2.5-pro",
        canonical: "google/gemini-2.5-pro",
        name: "Gemini 2.5 Pro",
        reasoning: true,
        limits: 1_048_576 / 65_536,
        pricing: tiered(1_250, 10_000, 125, 0, PricingTierDefinition {
            above_input_tokens: 200_000, input: 2_500, output: 15_000,
            cache_read: Some(250), cache_write: None,
        })
    },
    model! { catalogs: GOOGLE_API, wire: "gemini-2.5-flash", canonical: "google/gemini-2.5-flash", name: "Gemini 2.5 Flash", reasoning: true, limits: 1_048_576 / 65_536, pricing: metered(300, 2_500, 30, 0) },
    model! { catalogs: GOOGLE_API, wire: "gemini-2.5-flash-lite", canonical: "google/gemini-2.5-flash-lite", name: "Gemini 2.5 Flash-Lite", reasoning: true, limits: 1_048_576 / 65_536, pricing: metered(100, 400, 10, 0) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "us.anthropic.claude-sonnet-5", canonical: "anthropic/claude-sonnet-5", name: "Claude Sonnet 5 (US)", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(2_000, 10_000, 200, 2_500) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "global.anthropic.claude-sonnet-5", canonical: "anthropic/claude-sonnet-5", name: "Claude Sonnet 5 (Global)", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(2_000, 10_000, 200, 2_500) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "anthropic.claude-sonnet-5", canonical: "anthropic/claude-sonnet-5", name: "Claude Sonnet 5", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(2_000, 10_000, 200, 2_500) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "us.anthropic.claude-opus-4-8", canonical: "anthropic/claude-opus-4-8", name: "Claude Opus 4.8 (US)", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(5_000, 25_000, 500, 6_250) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "global.anthropic.claude-opus-4-8", canonical: "anthropic/claude-opus-4-8", name: "Claude Opus 4.8 (Global)", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(5_000, 25_000, 500, 6_250) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "us.anthropic.claude-opus-4-7", canonical: "anthropic/claude-opus-4-7", name: "Claude Opus 4.7 (US)", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(5_000, 25_000, 500, 6_250) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "us.anthropic.claude-sonnet-4-6", canonical: "anthropic/claude-sonnet-4-6", name: "Claude Sonnet 4.6 (US)", reasoning: true, limits: 1_000_000 / 64_000, pricing: metered(3_000, 15_000, 300, 3_750) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "global.anthropic.claude-sonnet-4-6", canonical: "anthropic/claude-sonnet-4-6", name: "Claude Sonnet 4.6 (Global)", reasoning: true, limits: 1_000_000 / 64_000, pricing: metered(3_000, 15_000, 300, 3_750) },
    model! { catalogs: BEDROCK_RUNTIME, wire: "us.anthropic.claude-haiku-4-5-20251001-v1:0", canonical: "anthropic/claude-haiku-4-5", name: "Claude Haiku 4.5 (US)", reasoning: true, limits: 200_000 / 64_000, pricing: metered(1_000, 5_000, 100, 1_250) },
    model! { catalogs: BEDROCK_MANTLE, wire: "anthropic.claude-opus-5", canonical: "anthropic/claude-opus-5", name: "Claude Opus 5", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(5_000, 25_000, 0, 0), api: ProviderApi::AnthropicMessages },
    model! { catalogs: BEDROCK_MANTLE, wire: "anthropic.claude-fable-5", canonical: "anthropic/claude-fable-5", name: "Claude Fable 5", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(10_000, 50_000, 0, 0), api: ProviderApi::AnthropicMessages },
    model! { catalogs: BEDROCK_MANTLE, wire: "anthropic.claude-sonnet-5", canonical: "anthropic/claude-sonnet-5", name: "Claude Sonnet 5", reasoning: true, limits: 1_000_000 / 128_000, pricing: metered(2_000, 10_000, 0, 0), api: ProviderApi::AnthropicMessages },
    model! { catalogs: BEDROCK_MANTLE, wire: "openai.gpt-5.6-sol", canonical: "openai/gpt-5.6-sol", name: "GPT-5.6 Sol", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: metered(5_500, 33_000, 0, 0), api: ProviderApi::OpenAiResponses },
    model! { catalogs: BEDROCK_MANTLE, wire: "openai.gpt-5.6-luna", canonical: "openai/gpt-5.6-luna", name: "GPT-5.6 Luna", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: metered(1_100, 6_600, 0, 0), api: ProviderApi::OpenAiResponses },
    model! { catalogs: BEDROCK_MANTLE, wire: "openai.gpt-5.6-terra", canonical: "openai/gpt-5.6-terra", name: "GPT-5.6 Terra", reasoning: true, efforts: OPENAI_EFFORTS, limits: 272_000 / 128_000, pricing: metered(2_200, 13_200, 0, 0), api: ProviderApi::OpenAiResponses },
    model! { catalogs: XAI_API, wire: "grok-4.7", canonical: "xai/grok-4.7", name: "Grok 4.7", reasoning: true, efforts: XAI_EFFORTS, limits: 500_000 / 500_000, pricing: tiered(2_000, 6_000, 500, 0, PricingTierDefinition { above_input_tokens: 200_000, input: 4_000, output: 12_000, cache_read: Some(1_000), cache_write: None }), api: ProviderApi::OpenAiResponses },
    model! { catalogs: XAI_API, wire: "grok-4.6", canonical: "xai/grok-4.6", name: "Grok 4.6", reasoning: true, efforts: XAI_EFFORTS, limits: 500_000 / 500_000, pricing: tiered(2_000, 6_000, 500, 0, PricingTierDefinition { above_input_tokens: 200_000, input: 4_000, output: 12_000, cache_read: Some(1_000), cache_write: None }), api: ProviderApi::OpenAiResponses },
    model! { catalogs: XAI_API, wire: "grok-4.5", canonical: "xai/grok-4.5", name: "Grok 4.5", reasoning: true, efforts: XAI_EFFORTS, limits: 256_000 / 128_000, pricing: None, api: ProviderApi::OpenAiResponses },
    model! { catalogs: XAI_API, wire: "grok-4.3", canonical: "xai/grok-4.3", name: "Grok 4.3", reasoning: true, efforts: XAI_EFFORTS, limits: 131_072 / 32_768, pricing: None, api: ProviderApi::OpenAiChatCompletions },
];

pub(crate) fn builtin_models(catalog: BuiltinCatalog) -> BTreeMap<String, ModelMetadata> {
    MODELS
        .iter()
        .filter(|model| model.catalogs & catalog.mask() != 0)
        .map(|model| {
            let pricing = model.pricing.map(|pricing| ModelPricing {
                input_usd_nanos_per_token: pricing.input,
                output_usd_nanos_per_token: pricing.output,
                cache_read_usd_nanos_per_token: if pricing.cache_read == 0 {
                    None
                } else {
                    Some(pricing.cache_read)
                },
                cache_write_usd_nanos_per_token: pricing.cache_write,
                context_tier: pricing.context_tier.map(|tier| ModelPricingTier {
                    above_input_tokens: tier.above_input_tokens,
                    input_usd_nanos_per_token: tier.input,
                    output_usd_nanos_per_token: tier.output,
                    cache_read_usd_nanos_per_token: tier.cache_read,
                    cache_write_usd_nanos_per_token: tier.cache_write,
                }),
                provenance: PROVENANCE.to_owned(),
            });
            (
                model.wire_id.to_owned(),
                ModelMetadata::builtin(
                    model.canonical_id,
                    model.api,
                    model.name,
                    model.reasoning,
                    model.context_tokens,
                    model.output_tokens,
                    pricing,
                )
                .with_reasoning_efforts(model.efforts),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt6_sol_and_luna_have_current_api_limits_and_no_minimal_effort() {
        for catalog in [BuiltinCatalog::OpenAiApi, BuiltinCatalog::OpenAiCodex] {
            let models = builtin_models(catalog);
            for id in ["gpt-6-sol", "gpt-6-luna"] {
                let model = &models[id];
                assert_eq!(model.canonical_id(), Some(format!("openai/{id}").as_str()));
                assert_eq!(model.max_output_tokens(), Some(128_000));
                assert_eq!(model.reasoning_efforts(), GPT6_EFFORTS);
                assert!(!model.explicitly_configured());
            }
        }
        let models = builtin_models(BuiltinCatalog::OpenAiApi);
        assert_eq!(models["gpt-6-sol"].context_window(), Some(1_050_000));
        assert_eq!(models["gpt-6-luna"].context_window(), Some(1_050_000));
    }

    #[test]
    fn access_routes_share_canonical_model_identity() {
        let anthropic = builtin_models(BuiltinCatalog::AnthropicApi);
        let bedrock = builtin_models(BuiltinCatalog::BedrockRuntime);

        assert_eq!(
            anthropic["claude-sonnet-5"].canonical_id(),
            bedrock["us.anthropic.claude-sonnet-5"].canonical_id()
        );
    }

    #[test]
    fn current_frontier_models_are_available_through_direct_and_codex_routes() {
        let openai = builtin_models(BuiltinCatalog::OpenAiApi);
        let codex = builtin_models(BuiltinCatalog::OpenAiCodex);
        let xai = builtin_models(BuiltinCatalog::XAiApi);

        assert_eq!(
            openai["gpt-6-astra"].canonical_id(),
            Some("openai/gpt-6-astra")
        );
        assert_eq!(
            codex["gpt-6-astra"].canonical_id(),
            Some("openai/gpt-6-astra")
        );
        assert_eq!(xai["grok-4.7"].canonical_id(), Some("xai/grok-4.7"));
        assert_eq!(xai["grok-4.7"].api(), Some(ProviderApi::OpenAiResponses));
        assert_eq!(xai["grok-4.7"].name(), Some("Grok 4.7"));
        assert!(xai["grok-4.7"].reasoning());
        assert_eq!(
            xai["grok-4.7"].reasoning_efforts(),
            &[ReasoningEffort::Low, ReasoningEffort::High]
        );
        assert_eq!(xai["grok-4.7"].context_window(), Some(500_000));
        assert_eq!(xai["grok-4.7"].max_output_tokens(), Some(500_000));
        let pricing = xai["grok-4.7"].pricing().expect("grok-4.7 ships pricing");
        assert_eq!(pricing.input_usd_nanos_per_token, 2_000);
        assert_eq!(pricing.output_usd_nanos_per_token, 6_000);
        assert_eq!(pricing.cache_read_usd_nanos_per_token, Some(500));
        let tier = pricing
            .context_tier
            .as_ref()
            .expect("grok-4.7 prices long context separately");
        assert_eq!(tier.above_input_tokens, 200_000);
        assert_eq!(tier.input_usd_nanos_per_token, 4_000);
        assert_eq!(tier.output_usd_nanos_per_token, 12_000);
        assert_eq!(pricing.provenance, PROVENANCE);
    }

    /// The catalog advertises an effort ladder only where the wire adapter
    /// transmits `reasoning_effort`. Anthropic-shaped routes never do, so
    /// their entries stay empty even though the models reason.
    #[test]
    fn effort_ladders_follow_the_wire_adapter_not_the_reasoning_flag() {
        let openai = builtin_models(BuiltinCatalog::OpenAiApi);
        let codex = builtin_models(BuiltinCatalog::OpenAiCodex);
        let anthropic = builtin_models(BuiltinCatalog::AnthropicApi);
        let mantle = builtin_models(BuiltinCatalog::BedrockMantle);

        assert_eq!(openai["gpt-6-astra"].reasoning_efforts(), OPENAI_EFFORTS);
        assert_eq!(codex["gpt-6-astra"].reasoning_efforts(), OPENAI_EFFORTS);
        assert!(
            openai["gpt-6-astra"]
                .reasoning_efforts()
                .contains(&ReasoningEffort::Xhigh)
        );
        assert!(openai["gpt-4.1"].reasoning_efforts().is_empty());
        assert!(!openai["gpt-4.1"].reasoning());

        assert!(anthropic["claude-sonnet-5"].reasoning());
        assert!(anthropic["claude-sonnet-5"].reasoning_efforts().is_empty());

        assert_eq!(
            mantle["openai.gpt-5.6-sol"].reasoning_efforts(),
            OPENAI_EFFORTS
        );
        assert!(
            mantle["anthropic.claude-opus-5"]
                .reasoning_efforts()
                .is_empty()
        );
    }

    #[test]
    fn xai_models_select_protocol_per_deployment() {
        let xai = builtin_models(BuiltinCatalog::XAiApi);
        assert_eq!(xai["grok-4.7"].api(), Some(ProviderApi::OpenAiResponses));
        assert_eq!(xai["grok-4.6"].api(), Some(ProviderApi::OpenAiResponses));
        assert_eq!(xai["grok-4.5"].api(), Some(ProviderApi::OpenAiResponses));
        assert_eq!(
            xai["grok-4.3"].api(),
            Some(ProviderApi::OpenAiChatCompletions)
        );
    }

    #[test]
    fn bedrock_mantle_models_select_protocol_per_vendor() {
        let mantle = builtin_models(BuiltinCatalog::BedrockMantle);

        for wire in [
            "openai.gpt-5.6-sol",
            "openai.gpt-5.6-luna",
            "openai.gpt-5.6-terra",
        ] {
            assert_eq!(mantle[wire].api(), Some(ProviderApi::OpenAiResponses));
        }
        for wire in [
            "anthropic.claude-opus-5",
            "anthropic.claude-sonnet-5",
            "anthropic.claude-fable-5",
        ] {
            assert_eq!(mantle[wire].api(), Some(ProviderApi::AnthropicMessages));
        }
        assert!(!mantle.contains_key("openai.gpt-oss-120b"));
    }
}
