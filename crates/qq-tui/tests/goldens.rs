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

#[test]
fn frames_fill_every_golden_size_with_the_dock_on_the_last_rows() {
    for &(width, height) in GOLDEN_SIZES {
        let mut harness = BenchHarness::scene(Scene::GoldenPath, (width, height));
        let rows = harness.plain_frame();
        assert_eq!(rows.len(), usize::from(height), "{width}x{height}");
        let composer = &rows[rows.len() - 2];
        assert!(
            composer.starts_with("  ┃ Ask QQ…"),
            "{width}x{height}: composer row is {composer:?}"
        );
        let rule = &rows[rows.len() - 3];
        assert!(
            rule.trim_start().contains("───"),
            "{width}x{height}: rule row is {rule:?}"
        );
    }
}

#[test]
fn the_redesign_has_no_header_label_strip_or_permanent_hints() {
    for scene in Scene::ALL {
        for &size in GOLDEN_SIZES {
            let rows = BenchHarness::scene(scene, size).plain_frame();
            for row in &rows {
                for banned in [
                    "▌ YOU",
                    "^K commands",
                    "ttft",
                    "Alt-R",
                    "› Ask",
                    " qq  Session",
                    "approval needed",
                ] {
                    assert!(
                        !row.contains(banned),
                        "{scene:?} {size:?} shows {banned:?}: {row:?}"
                    );
                }
            }
        }
    }
}

fn transcript_rows(size: (u16, u16)) -> usize {
    BenchHarness::scene(Scene::GoldenPath, size)
        .plain_frame()
        .iter()
        .rposition(|row| row.contains("───"))
        .expect("the rule row")
}

#[test]
fn dropping_the_header_and_strip_gives_the_transcript_more_rows() {
    let compact_v0_1_11 = 24 - 1 - 1 - 3;
    assert!(
        transcript_rows((80, 24)) >= compact_v0_1_11 + 2,
        "80x24 transcript has {} rows; v0.1.11 had {compact_v0_1_11}",
        transcript_rows((80, 24))
    );
    let regular_v0_1_11 = 40 - 1 - 3;
    assert!(
        transcript_rows((120, 40)) > regular_v0_1_11,
        "120x40 transcript has {} rows; v0.1.11 had {regular_v0_1_11}",
        transcript_rows((120, 40))
    );
}

#[test]
fn prose_starts_at_the_two_column_margin_at_every_width() {
    for &size in GOLDEN_SIZES {
        let rows = BenchHarness::scene(Scene::MarkdownGallery, size).plain_frame();
        let paragraph = rows
            .iter()
            .find(|row| row.contains("Tail paragraph."))
            .expect("gallery tail paragraph");
        let indent = paragraph.len() - paragraph.trim_start().len();
        assert_eq!(indent, 4, "{size:?}: {paragraph:?}");
    }
}

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
    assert_eq!(
        embedded,
        qq_tui::bench_support::MARKDOWN_GALLERY,
        "docs/runbooks/tui-qa.md GALLERY drifted from bench_support::MARKDOWN_GALLERY"
    );
}
