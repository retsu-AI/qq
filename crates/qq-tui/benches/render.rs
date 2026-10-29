use std::{hint::black_box, time::Instant};

use qq_tui::bench_support::BenchHarness;

const SIZE: (u16, u16) = (160, 48);
const DEFAULT_ITERATIONS: u32 = 2_000;
const WARMUP: u32 = 200;
const STEADY_MESSAGES: u8 = 64;
const BACKGROUND_SESSIONS: u8 = 8;
const DELTA: &str = "streamed text that keeps arriving in small pieces and eventually forms a paragraph of prose.\n\n";
const RUN_ON_DELTA: &str = "streamed text that keeps arriving in small pieces ";
const LIVE_PREFILL: usize = 340;

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = runtime.enter();

    steady_state(iterations);
    steady_state_with_sidebar(iterations);
    streaming_focused(iterations);
    streaming_run_on(iterations);
    streaming_background(iterations);
    children_with_sidebar(iterations);
    keystroke_echo(iterations);
    wheel_scroll(iterations);
    golden_path_first_minute(iterations);
    tool_calls_32(iterations);
    sessions_200_with_sidebar(iterations);
    picker_open_close(iterations);
    resize_horizontal(iterations);
    compact_80x24(iterations);
    wide_160x48_full(iterations);
    resize_ultra(iterations);
}

fn steady_state(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 1, STEADY_MESSAGES);
    harness.hide_sidebar();
    harness.hide_inspector();
    let first = timed(|| harness.draw());
    report("steady_state_first_frame_plain", first.0, 1, first.1);
    let started = Instant::now();
    let mut frames = 1;
    loop {
        let applied = harness.apply_finished_highlights();
        if applied > 0 {
            black_box(harness.draw());
            frames += 1;
        }
        if !harness.highlights_pending() && applied == 0 {
            black_box(harness.draw());
            if !harness.highlights_pending() {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
    println!(
        "steady_state_fully_highlighted: {} after {frames} frames",
        micros(started.elapsed().as_nanos())
    );
    let samples = collect(iterations, || harness.draw().len());
    report_samples("steady_state_frame", &samples);
}

fn steady_state_with_sidebar(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 9, STEADY_MESSAGES);
    harness.show_sidebar();
    harness.hide_inspector();
    black_box(harness.draw());
    let samples = collect(iterations, || harness.draw().len());
    report_samples("steady_state_with_sidebar_frame", &samples);
}

fn streaming_focused(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 1, STEADY_MESSAGES);
    harness.hide_sidebar();
    harness.hide_inspector();
    let message = harness.start_stream(0);
    for _ in 0..LIVE_PREFILL {
        harness.append(0, message, DELTA);
    }
    black_box(harness.draw());
    let samples = collect(iterations, || {
        harness.append(0, message, DELTA);
        harness.draw().len()
    });
    report_samples("streaming_focused_delta_to_frame", &samples);
}

fn streaming_run_on(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 1, STEADY_MESSAGES);
    harness.hide_sidebar();
    harness.hide_inspector();
    let message = harness.start_stream(0);
    for _ in 0..LIVE_PREFILL * 2 {
        harness.append(0, message, RUN_ON_DELTA);
    }
    black_box(harness.draw());
    let samples = collect(iterations, || {
        harness.append(0, message, RUN_ON_DELTA);
        harness.draw().len()
    });
    report_samples("streaming_run_on_32kb_delta_to_frame", &samples);
}

fn streaming_background(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, BACKGROUND_SESSIONS + 1, STEADY_MESSAGES);
    harness.hide_sidebar();
    harness.hide_inspector();
    let messages: Vec<_> = (1..=BACKGROUND_SESSIONS)
        .map(|index| (index, harness.start_stream(index)))
        .collect();
    black_box(harness.draw());
    let samples = collect(iterations, || {
        for (index, message) in &messages {
            harness.append(*index, *message, DELTA);
        }
        harness.draw().len()
    });
    report_samples("streaming_background_8_delta_to_frame", &samples);
}

fn children_with_sidebar(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 21, STEADY_MESSAGES);
    harness.show_sidebar();
    harness.hide_inspector();
    let messages: Vec<_> = (1..=20)
        .map(|index| (index, harness.start_stream(index)))
        .collect();
    black_box(harness.draw());
    let samples = collect(iterations, || {
        for (index, message) in &messages {
            harness.append(*index, *message, DELTA);
        }
        harness.draw().len()
    });
    report_samples("children_20_with_sidebar_delta_to_frame", &samples);
}

fn keystroke_echo(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 1, STEADY_MESSAGES);
    harness.hide_sidebar();
    harness.hide_inspector();
    black_box(harness.draw());
    let mut alphabet = ('a'..='z').cycle();
    let samples = collect(iterations, || {
        let character = alphabet.next().expect("cycle is infinite");
        harness.keystroke(character);
        harness.draw().len()
    });
    report_samples("keystroke_to_frame", &samples);
}

fn tool_calls_32(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 1, 4);
    harness.hide_sidebar();
    harness.hide_inspector();
    harness.add_tool_calls(32);
    black_box(harness.draw());
    let samples = collect(iterations, || harness.draw().len());
    report_samples("tool_calls_32_rows_frame", &samples);
    harness.fold_tools();
    black_box(harness.draw());
    let samples = collect(iterations, || harness.draw().len());
    report_samples("tool_calls_32_folded_frame", &samples);
    harness.expand_every_tool();
    black_box(harness.draw());
    let samples = collect(iterations, || harness.draw().len());
    report_samples("tool_calls_32_expanded_frame", &samples);
    harness.show_inspector();
    black_box(harness.draw());
    let samples = collect(iterations, || harness.draw().len());
    report_samples("tool_calls_32_expanded_inspector_frame", &samples);
}

fn sessions_200_with_sidebar(iterations: u32) {
    let mut harness = BenchHarness::with_sessions(SIZE, 200);
    harness.show_sidebar();
    harness.hide_inspector();
    black_box(harness.draw());
    let samples = collect(iterations, || harness.draw().len());
    report_samples("sessions_200_with_sidebar_frame", &samples);
}

fn picker_open_close(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 4, STEADY_MESSAGES);
    harness.hide_sidebar();
    harness.hide_inspector();
    harness.settle_highlights();
    let samples = collect(iterations, || {
        harness.open_and_close_session_picker();
        0
    });
    report_samples("picker_open_close_cycle", &samples);
}

fn resize_horizontal(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 1, STEADY_MESSAGES);
    harness.hide_sidebar();
    harness.hide_inspector();
    harness.settle_highlights();
    let mut delta = 1;
    let samples = collect(iterations, || {
        delta = -delta;
        harness.resize(delta).len()
    });
    report_samples("resize_horizontal_full_frame", &samples);
}

fn compact_80x24(iterations: u32) {
    let mut harness = BenchHarness::new((80, 24), 3, STEADY_MESSAGES);
    harness.settle_highlights();
    black_box(harness.draw());
    let samples = collect(iterations, || harness.draw().len());
    report_samples("compact_80x24_frame", &samples);
}

fn wide_160x48_full(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 9, STEADY_MESSAGES);
    harness.show_inspector();
    harness.settle_highlights();
    black_box(harness.draw());
    let samples = collect(iterations, || harness.draw().len());
    report_samples("wide_160x48_full_frame", &samples);
}

fn resize_ultra(iterations: u32) {
    let mut harness = BenchHarness::new((480, 120), 3, STEADY_MESSAGES);
    harness.settle_highlights();
    let mut delta = 1;
    let samples = collect(iterations, || {
        delta = -delta;
        harness.resize(delta).len()
    });
    report_samples("resize_ultra_480x120_full_frame", &samples);
}

fn timed<T>(mut work: impl FnMut() -> T) -> (u128, T) {
    let started = Instant::now();
    let value = work();
    (started.elapsed().as_nanos(), value)
}

fn collect(iterations: u32, mut work: impl FnMut() -> usize) -> Vec<u128> {
    for _ in 0..WARMUP {
        black_box(work());
    }
    (0..iterations)
        .map(|_| {
            let started = Instant::now();
            black_box(work());
            started.elapsed().as_nanos()
        })
        .collect()
}

fn report_samples(name: &str, samples: &[u128]) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let percentile = |fraction: f64| {
        let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
        sorted[index]
    };
    println!(
        "{name}: median={} p95={} p99={} ({} samples)",
        micros(percentile(0.5)),
        micros(percentile(0.95)),
        micros(percentile(0.99)),
        sorted.len()
    );
}

fn report(name: &str, nanos: u128, samples: usize, bytes: Vec<u8>) {
    println!(
        "{name}: {} ({samples} sample, {} frame bytes)",
        micros(nanos),
        bytes.len()
    );
}

fn micros(nanos: u128) -> String {
    format!("{:.1} us", nanos as f64 / 1_000.0)
}

fn wheel_scroll(iterations: u32) {
    let mut harness = BenchHarness::new(SIZE, 1, STEADY_MESSAGES);
    harness.hide_sidebar();
    harness.hide_inspector();
    harness.settle_highlights();
    black_box(harness.draw());
    let mut up = true;
    let samples = collect(iterations, || {
        for _ in 0..4 {
            if up {
                harness.wheel_up();
            } else {
                harness.wheel_down();
            }
        }
        up = !up;
        harness.draw().len()
    });
    report_samples("wheel_scroll_4_rows_to_frame", &samples);
}

fn golden_path_first_minute(iterations: u32) {
    let samples = collect(iterations, || {
        let mut harness = BenchHarness::new(SIZE, 3, 0);
        harness.hide_sidebar();
        harness.hide_inspector();
        black_box(harness.draw());
        let message = harness.golden_path();
        let mut bytes = harness.draw().len();
        harness.append(
            0,
            message,
            " Then the reconnect test can assert on virtual time.",
        );
        bytes += harness.draw().len();
        bytes
    });
    report_samples("golden_path_first_minute_total", &samples);
}
