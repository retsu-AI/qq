//! Cost of `search` and `tree` over a synthetic 10k-file workspace: an
//! ignore-aware walk plus content scan in the page cache.
//!
//! Budget (per `docs/plans/tool-layer.md` T2): a hot content search over
//! 10k files ≤ 150 ms. The fixture mirrors a mid-size repository: 200
//! directories × 50 source files of ~40 lines, a `.gitignore`, and a
//! `target/` tree of the same size that the walk must skip without reading.

use std::{hint::black_box, time::Instant};

use qq_core::tool_bench as bench;

const DEFAULT_ITERATIONS: u64 = 20;
const DIRECTORIES: usize = 200;
const FILES_PER_DIRECTORY: usize = 50;

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

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let directory = tempfile::tempdir().expect("temporary workspace must be created");
    let root = directory.path();
    std::fs::write(root.join(".gitignore"), "*.log\n").expect("gitignore must be written");
    for tree in ["src", "target"] {
        for d in 0..DIRECTORIES {
            let dir = root.join(tree).join(format!("module_{d:03}"));
            std::fs::create_dir_all(&dir).expect("directory must be created");
            for f in 0..FILES_PER_DIRECTORY {
                let mut body = String::with_capacity(2_048);
                for line in 0..40 {
                    body.push_str(&format!(
                        "    let value_{d}_{f}_{line} = compute_{line}(input, {line});\n"
                    ));
                }
                if f == 0 {
                    body.push_str(
                        "pub fn apply_lock(&self) -> Guard {\n    apply_lock_inner()\n}\n",
                    );
                }
                std::fs::write(dir.join(format!("file_{f:03}.rs")), body)
                    .expect("file must be written");
            }
            std::fs::write(dir.join("debug.log"), "apply_lock in a log\n")
                .expect("log must be written");
        }
    }

    // No match anywhere: the walk reads all 10k files (the gate case).
    let header = measure("content_absent_10k_full_scan", iterations, || {
        bench::run_tool(root, "search", r#"{"query":"no_such_symbol_anywhere"}"#)
    });
    println!("  {}", header.lines().next().unwrap_or_default());
    let header = measure("content_rare_10k", iterations, || {
        bench::run_tool(root, "search", r#"{"query":"apply_lock_inner"}"#)
    });
    println!("  {}", header.lines().next().unwrap_or_default());
    let header = measure("content_common_10k_first_page", iterations, || {
        bench::run_tool(root, "search", r#"{"query":"compute_7"}"#)
    });
    println!("  {}", header.lines().next().unwrap_or_default());
    let header = measure("definition_10k", iterations, || {
        bench::run_tool(
            root,
            "search",
            r#"{"query":"apply_lock","mode":"definition"}"#,
        )
    });
    println!("  {}", header.lines().next().unwrap_or_default());
    let header = measure("references_10k_full_scan", iterations, || {
        bench::run_tool(
            root,
            "search",
            r#"{"query":"absent_symbol","mode":"references"}"#,
        )
    });
    println!("  {}", header.lines().next().unwrap_or_default());
    let header = measure("definition_10k_full_scan", iterations, || {
        bench::run_tool(
            root,
            "search",
            r#"{"query":"absent_symbol","mode":"definition"}"#,
        )
    });
    println!("  {}", header.lines().next().unwrap_or_default());
    let header = measure("names_10k", iterations, || {
        bench::run_tool(root, "search", r#"{"query":"file_049","mode":"names"}"#)
    });
    println!("  {}", header.lines().next().unwrap_or_default());
    let header = measure("tree_depth2", iterations, || {
        bench::run_tool(root, "tree", r#"{"depth":2}"#)
    });
    println!("  {}", header.lines().next().unwrap_or_default());
}
