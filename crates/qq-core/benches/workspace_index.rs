//! Cost of the Merkle workspace index over the same synthetic 10k-file
//! workspace `search_walk` uses (`docs/plans/run-snapshots.md` § Change
//! Detection).
//!
//! Three numbers matter for a per-turn checkpoint: `build_10k` is the cold
//! cost (walk + read + SHA-256 of every file); `refresh_10k_quiet` is the
//! steady-state cost on an unchanged tree (walk only, every hash reused);
//! `refresh_10k_one_edit` is the common case (walk plus one file read).
//! `diff_10k` is reported alongside because a checkpoint calls it once.
//! A refresh that reads is the regression this bench exists to catch:
//! `reused` must equal the file count on the quiet run.

use std::{hint::black_box, time::Instant};

use qq_core::{IndexBudget, IndexOutcome, WorkspaceIndex};

const DEFAULT_ITERATIONS: u64 = 10;
const DIRECTORIES: usize = 200;
const FILES_PER_DIRECTORY: usize = 50;

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let directory = tempfile::tempdir().expect("temporary workspace must be created");
    let root = std::fs::canonicalize(directory.path()).expect("workspace must canonicalize");
    std::fs::write(root.join(".gitignore"), "*.log\n").expect("gitignore must be written");
    for tree in ["src", "target"] {
        for d in 0..DIRECTORIES {
            let dir = root.join(tree).join(format!("module_{d:03}"));
            std::fs::create_dir_all(&dir).expect("directory must be created");
            for f in 0..FILES_PER_DIRECTORY {
                let mut body = String::with_capacity(2_048);
                for line in 0..40 {
                    if line % 12 == 11 {
                        body.push('\n');
                        continue;
                    }
                    body.push_str(&format!(
                        "    let value_{d}_{f}_{line} = compute_{line}(input, {line});\n"
                    ));
                }
                std::fs::write(dir.join(format!("file_{f:03}.rs")), body)
                    .expect("file must be written");
            }
            std::fs::write(dir.join("debug.log"), "ignored\n").expect("log must be written");
        }
    }
    let budget = IndexBudget::default();
    let complete = |outcome: IndexOutcome| match outcome {
        IndexOutcome::Complete(index) => index,
        IndexOutcome::Partial(partial) => {
            panic!("the 10k fixture must fit the default budget: {partial:?}")
        }
    };
    // Let the fixture's mtimes fall strictly behind the first index's clock
    // so the quiet refresh measures reuse, not the same-tick guard.
    std::thread::sleep(std::time::Duration::from_millis(20));

    let mut last = None;
    for _ in 0..2 {
        last = Some(complete(
            WorkspaceIndex::build(&root, &budget).expect("index must build"),
        ));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        last = Some(complete(black_box(
            WorkspaceIndex::build(&root, &budget).expect("index must build"),
        )));
    }
    let per_build = started.elapsed() / u32::try_from(iterations).unwrap_or(u32::MAX);
    let index = last.expect("at least one build ran");
    println!(
        "build_10k: {} µs/iteration ({iterations} iterations)",
        per_build.as_micros()
    );
    println!(
        "  entries={} files={} bytes_read={} entries/s={:.0}",
        index.entries,
        index.files.len(),
        index.bytes_read,
        index.entries as f64 / per_build.as_secs_f64(),
    );

    let started = Instant::now();
    let mut quiet = None;
    for _ in 0..iterations {
        quiet = Some(complete(black_box(
            index.refresh(&root, &budget).expect("refresh must build"),
        )));
    }
    let per_quiet = started.elapsed() / u32::try_from(iterations).unwrap_or(u32::MAX);
    let quiet = quiet.expect("at least one refresh ran");
    println!(
        "refresh_10k_quiet: {} µs/iteration ({iterations} iterations)",
        per_quiet.as_micros()
    );
    println!(
        "  reused={} bytes_read={} same_root={}",
        quiet.reused,
        quiet.bytes_read,
        quiet.root_hash == index.root_hash
    );
    assert_eq!(
        quiet.reused,
        index.files.len(),
        "a quiet refresh must reuse every hash"
    );
    assert_eq!(quiet.root_hash, index.root_hash);

    std::fs::write(
        root.join("src/module_100/file_025.rs"),
        "    let changed = 1;\n",
    )
    .expect("edit must be written");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let started = Instant::now();
    let mut edited = None;
    for _ in 0..iterations {
        edited = Some(complete(black_box(
            index.refresh(&root, &budget).expect("refresh must build"),
        )));
    }
    let per_edit = started.elapsed() / u32::try_from(iterations).unwrap_or(u32::MAX);
    let after = edited.expect("at least one refresh ran");
    println!(
        "refresh_10k_one_edit: {} µs/iteration ({iterations} iterations)",
        per_edit.as_micros()
    );
    println!("  reused={} bytes_read={}", after.reused, after.bytes_read);
    assert_eq!(after.reused, index.files.len() - 1);

    let started = Instant::now();
    let mut diff = None;
    for _ in 0..iterations {
        diff = Some(black_box(index.diff(&after)));
    }
    let per_diff = started.elapsed() / u32::try_from(iterations).unwrap_or(u32::MAX);
    let diff = diff.expect("at least one diff ran");
    println!(
        "diff_10k_one_modified: {} µs/iteration ({iterations} iterations)",
        per_diff.as_micros()
    );
    println!(
        "  added={} modified={} deleted={}",
        diff.added.len(),
        diff.modified.len(),
        diff.deleted.len()
    );
    assert_eq!(diff.modified, ["src/module_100/file_025.rs"]);
}
