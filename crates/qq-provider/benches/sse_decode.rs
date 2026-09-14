//! SSE decoding cost: the bytes a provider streams back pass through
//! `SseDecoder` and then the adapter's JSON parse before anything else sees
//! them. This measures how much of that path is framing.
//!
//! Fixture: a realistic text-delta stream in each protocol's event shape
//! (OpenAI Responses `response.output_text.delta`, Anthropic
//! `content_block_delta`), at 64 KiB / 512 KiB / 1 MiB of body, delivered in
//! 16 KiB chunks the way a TCP body arrives. Each size reports:
//!
//! - `frame`: `SseDecoder::push` alone — the SSE state machine and the
//!   per-event `name`/`data` strings it allocates.
//! - `frame+parse`: the same plus the adapter's `decode_event`, which is the
//!   work the run loop actually waits on per event.
//! - allocation counts and bytes for each, from a counting allocator.
//!
//! D10 rewrites the framer only if `frame` is a material share of
//! `frame+parse`; this bench is the evidence either way.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use qq_provider::HttpProtocol;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System` unchanged; the counters are
// bookkeeping around the delegated call and never touch the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

const DEFAULT_ITERATIONS: u64 = 200;
const CHUNK_BYTES: usize = 16 * 1024;
const SIZES: [(&str, usize); 3] = [
    ("64KiB", 64 * 1024),
    ("512KiB", 512 * 1024),
    ("1MiB", 1024 * 1024),
];

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);

    for (protocol, name) in [
        (HttpProtocol::OpenAiResponses, "openai_responses"),
        (HttpProtocol::AnthropicMessages, "anthropic_messages"),
    ] {
        for (label, target) in SIZES {
            let body = body(protocol, target);
            let chunks: Vec<&[u8]> = body.chunks(CHUNK_BYTES).collect();
            let events = qq_provider::test_support::decode_sse_body(protocol, &chunks, false);

            let (frame_us, frame_allocs, frame_bytes) = measure(iterations, || {
                black_box(qq_provider::test_support::decode_sse_body(
                    protocol, &chunks, false,
                ))
            });
            let (full_us, full_allocs, full_bytes) = measure(iterations, || {
                black_box(qq_provider::test_support::decode_sse_body(
                    protocol, &chunks, true,
                ))
            });
            println!(
                "{name} {label}: {} body bytes, {events} events; frame {frame_us} us \
                 ({frame_allocs} allocs, {frame_bytes} bytes); frame+parse {full_us} us \
                 ({full_allocs} allocs, {full_bytes} bytes); framing share {:.0}%",
                body.len(),
                100.0 * frame_us as f64 / full_us.max(1) as f64
            );
        }
    }
}

/// Median-of-iterations microseconds plus per-iteration allocations/bytes.
fn measure(iterations: u64, mut run: impl FnMut() -> usize) -> (u128, usize, usize) {
    for _ in 0..5 {
        run();
    }
    let allocs_before = ALLOCS.load(Ordering::Relaxed);
    let bytes_before = BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    for _ in 0..iterations {
        run();
    }
    let elapsed = started.elapsed().as_micros() / u128::from(iterations);
    let allocs = (ALLOCS.load(Ordering::Relaxed) - allocs_before) / iterations as usize;
    let bytes = (BYTES.load(Ordering::Relaxed) - bytes_before) / iterations as usize;
    (elapsed, allocs, bytes)
}

/// A stream of small text deltas, the common case: ~20-40 bytes of text per
/// event wrapped in the protocol's envelope, terminated by the protocol's
/// completion event.
fn body(protocol: HttpProtocol, target: usize) -> Vec<u8> {
    let words = [
        "the ",
        "quick ",
        "brown ",
        "fox ",
        "jumps ",
        "over ",
        "a ",
        "lazy ",
        "dog. ",
        "Then ",
        "it ",
        "reads ",
        "src/lib.rs ",
        "and ",
        "edits ",
        "\"main\" ",
        "carefully.\n",
    ];
    let mut body = Vec::with_capacity(target + 512);
    let mut index = 0usize;
    match protocol {
        HttpProtocol::OpenAiResponses => {
            body.extend_from_slice(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            );
            while body.len() < target {
                let text = format!(
                    "{}{}",
                    words[index % words.len()],
                    words[(index * 7) % words.len()]
                );
                index += 1;
                body.extend_from_slice(b"event: response.output_text.delta\ndata: ");
                body.extend_from_slice(
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "item_id": "msg_1",
                        "output_index": 0,
                        "content_index": 0,
                        "delta": text,
                    })
                    .to_string()
                    .as_bytes(),
                );
                body.extend_from_slice(b"\n\n");
            }
            body.extend_from_slice(
                b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}}\n\n",
            );
        }
        HttpProtocol::AnthropicMessages => {
            body.extend_from_slice(
                b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            );
            body.extend_from_slice(
                b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            );
            while body.len() < target {
                let text = format!(
                    "{}{}",
                    words[index % words.len()],
                    words[(index * 7) % words.len()]
                );
                index += 1;
                body.extend_from_slice(b"event: content_block_delta\ndata: ");
                body.extend_from_slice(
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "text_delta", "text": text},
                    })
                    .to_string()
                    .as_bytes(),
                );
                body.extend_from_slice(b"\n\n");
            }
            body.extend_from_slice(
                b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            );
            body.extend_from_slice(
                b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":20}}\n\n",
            );
            body.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        }
        HttpProtocol::OpenAiChatCompletions | HttpProtocol::GoogleGenerateContent => {
            unreachable!("not benchmarked")
        }
    }
    body
}
