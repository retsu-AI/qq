//! Request encoding cost: the bytes and heap one provider request costs
//! between the transcript core holds and the HTTP body on the wire.
//!
//! Fixture: a one MiB transcript (D5's reference size) of alternating turns
//! with tool calls and results, plus 32 tool schemas, encoded for each HTTP
//! protocol. Two things are measured per protocol:
//!
//! - `encode`: time to build the wire struct and serialize the body from a
//!   `ModelRequest` the adapter already holds (`sse_exchange`'s `.json()`).
//! - `request heap`: peak heap above the transcript's own bytes while the
//!   adapter path runs — `ModelRequest::new` from core's transcript, the
//!   per-attempt `request.clone()` every adapter performs inside
//!   `with_restart`, and the body. D5's gate is a peak at most 2x the
//!   payload; before D5 the transcript alone existed three times.
//!
//! `QQ_BENCH_ITERATIONS` overrides the iteration count. Heap is measured with
//! a counting global allocator, so numbers are exact, not sampled.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use qq_provider::{ContentBlock, HttpProtocol, Message, ModelRequest, Role, ToolSpec};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System` unchanged; the counters are
// bookkeeping around the delegated call and never touch the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            record(new_size);
        }
        moved
    }
}

fn record(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn reset_peak() -> usize {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    live
}

const DEFAULT_ITERATIONS: u64 = 200;
const TARGET_TRANSCRIPT_BYTES: usize = 1024 * 1024;
const TOOL_COUNT: usize = 32;

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);

    let transcript = transcript();
    let tools = tools();
    let payload_bytes = transcript_bytes(&transcript);
    println!(
        "provider_encode fixture: {} messages, {payload_bytes} transcript bytes, {} tools",
        transcript.len(),
        tools.len()
    );

    for protocol in [
        HttpProtocol::OpenAiResponses,
        HttpProtocol::OpenAiChatCompletions,
        HttpProtocol::AnthropicMessages,
        HttpProtocol::GoogleGenerateContent,
    ] {
        let name = match protocol {
            HttpProtocol::OpenAiResponses => "openai_responses",
            HttpProtocol::OpenAiChatCompletions => "openai_chat",
            HttpProtocol::AnthropicMessages => "anthropic_messages",
            HttpProtocol::GoogleGenerateContent => "google_generate_content",
        };

        // Encode only: the body from a request the adapter already holds.
        let request = ModelRequest::new("benchmark-model", transcript.clone(), 4096)
            .with_tools(tools.clone())
            .with_system("You are a benchmark.");
        for _ in 0..10 {
            black_box(qq_provider::test_support::encode_body(protocol, &request));
        }
        let started = Instant::now();
        let mut body_bytes = 0;
        for _ in 0..iterations {
            let body = black_box(qq_provider::test_support::encode_body(protocol, &request));
            body_bytes = body.len();
        }
        let encode_us = started.elapsed().as_micros() / u128::from(iterations);
        drop(request);

        // Adapter path heap: core's transcript is live; build the request,
        // clone it as `with_restart` does per attempt, encode. Peak above the
        // transcript is the request's own cost. Measured twice: from a
        // caller that owns a `Vec` (copied into the request, as before D5)
        // and from one that shares an `Arc` (the run loop after D5).
        let baseline = reset_peak();
        let request = ModelRequest::new("benchmark-model", transcript.clone(), 4096)
            .with_tools(tools.clone())
            .with_system("You are a benchmark.");
        let attempt = request.clone();
        let body = qq_provider::test_support::encode_body(protocol, &attempt);
        let copied_peak = PEAK.load(Ordering::Relaxed) - baseline;
        black_box((&request, &attempt, &body));
        drop((body, attempt, request));

        let shared = std::sync::Arc::new(transcript.clone());
        let baseline = reset_peak();
        let request = ModelRequest::new("benchmark-model", std::sync::Arc::clone(&shared), 4096)
            .with_tools(tools.clone())
            .with_system("You are a benchmark.");
        let attempt = request.clone();
        let body = qq_provider::test_support::encode_body(protocol, &attempt);
        let shared_peak = PEAK.load(Ordering::Relaxed) - baseline;
        black_box((&request, &attempt, &body));
        drop((body, attempt, request, shared));

        println!(
            "{name}: encode {encode_us} us/iteration ({iterations} iterations), body {body_bytes} \
             bytes, request heap peak {copied_peak} bytes ({:.2}x payload) from an owned Vec, \
             {shared_peak} bytes ({:.2}x) from a shared Arc",
            copied_peak as f64 / payload_bytes as f64,
            shared_peak as f64 / payload_bytes as f64,
        );
    }
}

/// Alternating user/assistant turns with a tool call and result every other
/// turn, sized to about one MiB of text and arguments.
fn transcript() -> Vec<Message> {
    let mut messages = Vec::new();
    let mut bytes = 0;
    let mut turn = 0u32;
    while bytes < TARGET_TRANSCRIPT_BYTES {
        let user = Message::user(format!(
            "Turn {turn}: please inspect the module and report. {}",
            lorem(600)
        ));
        let assistant = Message::new(
            Role::Assistant,
            vec![
                ContentBlock::Text {
                    text: format!("Looking at turn {turn}. {}", lorem(400)),
                },
                ContentBlock::tool_call(
                    format!("call_{turn}"),
                    format!("tool_{}", turn as usize % TOOL_COUNT),
                    &serde_json::json!({
                        "path": format!("src/module_{turn}.rs"),
                        "query": lorem(120),
                        "limit": 200,
                        "flags": ["a", "b", "c"],
                    }),
                ),
            ],
        );
        let result = Message::tool_results(vec![ContentBlock::ToolResult {
            call_id: format!("call_{turn}"),
            content: lorem(1400),
            is_error: false,
        }]);
        for message in [user, assistant, result] {
            bytes += message_bytes(&message);
            messages.push(message);
        }
        turn += 1;
    }
    messages
}

fn tools() -> Vec<ToolSpec> {
    (0..TOOL_COUNT)
        .map(|index| {
            ToolSpec::new(
                format!("tool_{index}"),
                format!("Benchmark tool {index}: {}", lorem(80)),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": lorem(40)},
                        "query": {"type": "string", "description": lorem(40)},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 10000},
                        "flags": {"type": "array", "items": {"type": "string"}},
                    },
                    "required": ["path"],
                    "additionalProperties": false,
                }),
            )
        })
        .collect()
}

fn transcript_bytes(messages: &[Message]) -> usize {
    messages.iter().map(message_bytes).sum()
}

fn message_bytes(message: &Message) -> usize {
    message
        .content()
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => id.len() + name.len() + arguments.get().len(),
            ContentBlock::ToolResult {
                call_id, content, ..
            } => call_id.len() + content.len(),
        })
        .sum()
}

fn lorem(bytes: usize) -> String {
    const WORDS: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod \
                         tempor incididunt ut labore et dolore magna aliqua ";
    let mut text = String::with_capacity(bytes + WORDS.len());
    while text.len() < bytes {
        text.push_str(WORDS);
    }
    text.truncate(bytes);
    text
}
