//! Golden frames: the plain-text frame of every review scene at every
//! golden size, pinned under `tests/goldens/`. A layout or paint change
//! moves these on purpose, in the same PR, with the before and after in the
//! diff. Run with `QQ_UPDATE_GOLDENS=1` to rewrite them.
//!
//! Text only: styles are asserted by targeted tests so a palette change does
//! not move every golden.

use std::{fs, path::PathBuf};

use qq_tui::bench_support::{BenchHarness, GOLDEN_SIZES, Scene};

fn golden_path(scene: Scene, size: (u16, u16)) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/goldens")
        .join(format!("{}-{}x{}.txt", scene.name(), size.0, size.1))
}

fn render(scene: Scene, size: (u16, u16)) -> String {
    let mut harness = BenchHarness::scene(scene, size);
    let mut text = harness.plain_frame().join("\n");
    text.push('\n');
    text
}

#[test]
fn every_scene_matches_its_golden_at_every_size() {
    let update = std::env::var_os("QQ_UPDATE_GOLDENS").is_some();
    let mut mismatches = Vec::new();
    for scene in Scene::ALL {
        for &size in GOLDEN_SIZES {
            let path = golden_path(scene, size);
            let actual = render(scene, size);
            if update {
                fs::create_dir_all(path.parent().expect("goldens directory"))
                    .expect("create goldens directory");
                fs::write(&path, &actual).expect("write golden");
                continue;
            }
            match fs::read_to_string(&path) {
                Ok(expected) if expected == actual => {}
                Ok(expected) => mismatches.push(format!(
                    "{}: differs\n--- expected\n{expected}\n--- actual\n{actual}",
                    path.display()
                )),
                Err(error) => mismatches.push(format!(
                    "{}: {error}; run with QQ_UPDATE_GOLDENS=1 to record it",
                    path.display()
                )),
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} golden frame(s) differ; rerun with QQ_UPDATE_GOLDENS=1 if the change is intended\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

#[test]
fn every_golden_frame_fits_the_terminal() {
    for scene in Scene::ALL {
        for &(width, height) in GOLDEN_SIZES {
            let mut harness = BenchHarness::scene(scene, (width, height));
            let rows = harness.plain_frame();
            assert!(
                rows.len() <= usize::from(height),
                "{scene:?} at {width}x{height} produced {} rows",
                rows.len()
            );
            for (index, row) in rows.iter().enumerate() {
                let columns = row.chars().count();
                assert!(
                    columns <= usize::from(width),
                    "{scene:?} at {width}x{height} row {index} is {columns} columns wide: {row:?}"
                );
            }
        }
    }
}

/// The frame fills the terminal: at every golden size the widest row reaches
/// at least the second-to-last column (the top row's right-aligned status
/// ends one cell in), and where the layout shows a rail its border runs the
/// full body height at the last column. Before slice L1 a 480-column
/// terminal received a 320-wide frame with the rest blank.
#[test]
fn frames_fill_every_golden_size() {
    for &(width, height) in GOLDEN_SIZES {
        let mut harness = BenchHarness::scene(Scene::GoldenPath, (width, height));
        let rows = harness.plain_frame();
        assert_eq!(rows.len(), usize::from(height), "{width}x{height}");
        let widest = rows
            .iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0);
        assert!(
            widest + 1 >= usize::from(width),
            "{width}x{height}: widest row is {widest} columns"
        );
        if width >= 90 {
            // Body rows between the top row and the chrome all carry the rail.
            let body_rows = &rows[1..rows.len() - 3];
            assert!(
                body_rows
                    .iter()
                    .all(|row| row.chars().count() >= usize::from(width) - 28),
                "{width}x{height}: a body row stops before the rail"
            );
        }
    }
}

/// Prose lays out at the measure and is placed a third of the way into a
/// pane wider than it, so a full-screen terminal reads like a page rather
/// than a left-justified strip.
#[test]
fn prose_is_placed_at_the_measure_on_wide_terminals() {
    let mut harness = BenchHarness::scene(Scene::MarkdownGallery, (320, 90));
    let rows = harness.plain_frame();
    let first_paragraph = rows
        .iter()
        .find(|row| row.contains("First paragraph of prose."))
        .expect("gallery paragraph");
    let indent = first_paragraph.len() - first_paragraph.trim_start().len();
    // 320 - 28 rail = 292 pane; (292 - 100) / 3 = 64 inset, plus the
    // transcript's own 3-column prose rail.
    assert_eq!(indent, 64 + 3, "{first_paragraph:?}");
}

/// The QA runbook embeds the gallery in its fake endpoint so a real terminal
/// shows the same text the goldens pin. Keep the two copies identical.
#[test]
fn the_qa_runbook_streams_the_same_gallery_the_goldens_pin() {
    let runbook = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/runbooks/tui-qa.md");
    let text = fs::read_to_string(&runbook).expect("read docs/runbooks/tui-qa.md");
    let start = text
        .find("GALLERY = \"\"\"")
        .expect("runbook defines GALLERY")
        + "GALLERY = \"\"\"".len();
    let end = start + text[start..].find("\"\"\"").expect("GALLERY closes");
    let embedded = &text[start..end];
    // Python's `"{x}"` inside a triple-quoted string is literal; Rust's
    // constant escapes the quotes.
    assert_eq!(
        embedded,
        qq_tui::bench_support::MARKDOWN_GALLERY,
        "docs/runbooks/tui-qa.md GALLERY drifted from bench_support::MARKDOWN_GALLERY"
    );
}
