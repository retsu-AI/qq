//! Interface tests for the provider crate.
//!
//! Every test here enters through the crate's one seam —
//! `ProviderCompiler::compile` → `Provider::stream` — and observes only
//! public `ProviderEvent`/`ProviderError` values, so adapter internals can
//! move freely without touching these tests. Happy-path compile→stream
//! coverage with request capture lives in `src/compiler.rs`; these modules
//! add the per-protocol auth-failure and stream-limit matrix.

mod support;

mod anthropic;
mod canary;
mod google;
mod openai_chat;
mod openai_responses;
mod reasoning_effort;
