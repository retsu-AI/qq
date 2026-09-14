//! Cost of one `edit_file` batch: 32 edits over a 1 MiB source file, the
//! way a model rewrites a module in one call. Covers the cascade on every
//! edit (exact hits first; one drifted edit per strategy so the fuzzy tiers
//! run), the in-memory apply chain, the unified diff for the UI payload,
//! and the atomic temp+rename.
//!
//! Budget (per `docs/plans/tool-layer.md` T5): recorded here as the gate's
//! first measurement; the slice must not regress `tool_dispatch`.

use std::{hint::black_box, time::Instant};

use qq_core::tool_bench::Session;

const DEFAULT_ITERATIONS: u64 = 20;
const LINES: usize = 20_000;
const EDITS: usize = 32;

fn measure(name: &str, iterations: u64, mut run: impl FnMut() -> String) -> String {
    let mut last = String::new();
    for _ in 0..3 {
        last = run();
    }
    let started = Instant::now();
    for _ in 0..iterations {
        last = black_box(run());
    }
    let micros = started.elapsed().as_micros() / u128::from(iterations);
    println!("{name}: {micros} µs/iteration ({iterations} iterations)");
    last
}

fn source() -> String {
    let mut text = String::with_capacity(LINES * 52);
    for n in 0..LINES {
        text.push_str(&format!(
            "    fn item_{n:05}(value: u32) -> u32 {{ value + {n} }}\n"
        ));
    }
    text
}

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let directory = tempfile::tempdir().expect("temporary workspace must be created");
    let root = directory.path();
    let original = source();
    std::fs::write(root.join("big.rs"), &original).expect("fixture must be written");
    assert!(original.len() >= 1024 * 1024, "{} bytes", original.len());

    // 32 exact edits spread through the file, each unique.
    let exact: Vec<serde_json::Value> = (0..EDITS)
        .map(|i| {
            let n = i * (LINES / EDITS);
            serde_json::json!({
                "path": "big.rs",
                "old": format!("fn item_{n:05}(value: u32) -> u32 {{ value + {n} }}"),
                "new": format!("fn item_{n:05}(value: u32) -> u32 {{ value * 2 + {n} }}"),
            })
        })
        .collect();
    let exact_args = serde_json::json!({ "edits": exact, "dry_run": true }).to_string();

    // The same, with whitespace drift on every edit so each falls through to
    // a fuzzy tier (trailing blanks, collapsed inner spaces, de-indented).
    let drifted: Vec<serde_json::Value> = (0..EDITS)
        .map(|i| {
            let n = i * (LINES / EDITS);
            let old = match i % 3 {
                0 => format!("    fn item_{n:05}(value: u32) -> u32 {{ value + {n} }}   \n"),
                1 => format!("    fn  item_{n:05}(value:  u32) -> u32 {{ value + {n} }}\n"),
                _ => format!("fn item_{n:05}(value: u32) -> u32 {{ value + {n} }}\n"),
            };
            serde_json::json!({
                "path": "big.rs",
                "old": old,
                "new": format!("fn item_{n:05}(value: u32) -> u32 {{ value * 2 + {n} }}\n"),
            })
        })
        .collect();
    let drifted_args = serde_json::json!({ "edits": drifted, "dry_run": true }).to_string();

    let session = Session::open(root);
    let (error, _) = session.run("read_file", r#"{"path":"big.rs","limit":1}"#);
    assert!(!error);

    measure("edit_batch_32_exact_dry_run_1mib", iterations, || {
        let (error, text) = session.run("edit_file", &exact_args);
        assert!(!error, "{text}");
        text
    });
    measure("edit_batch_32_fuzzy_dry_run_1mib", iterations, || {
        let (error, text) = session.run("edit_file", &drifted_args);
        assert!(!error, "{text}");
        text
    });
    // The real apply: restore the file each round so every edit is found.
    let apply_args = exact_args.replace(r#","dry_run":true"#, "");
    measure("edit_batch_32_exact_apply_1mib", iterations, || {
        std::fs::write(root.join("big.rs"), &original).expect("fixture must be restored");
        let (error, _) = session.run("read_file", r#"{"path":"big.rs","limit":1}"#);
        assert!(!error);
        let (error, text) = session.run("edit_file", &apply_args);
        assert!(!error, "{text}");
        text
    });
    let single = serde_json::json!({ "edits": [exact_args_first()] , "dry_run": true }).to_string();
    measure("edit_single_exact_dry_run_1mib", iterations, || {
        let (error, text) = session.run("edit_file", &single);
        assert!(!error, "{text}");
        text
    });
}

fn exact_args_first() -> serde_json::Value {
    serde_json::json!({
        "path": "big.rs",
        "old": "fn item_00000(value: u32) -> u32 { value + 0 }",
        "new": "fn item_00000(value: u32) -> u32 { value * 2 + 0 }",
    })
}
