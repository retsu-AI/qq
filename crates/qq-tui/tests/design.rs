use std::{fs, path::PathBuf};

use qq_tui::{
    bench_support::{BenchHarness, GOLDEN_SIZES, Scene},
    design::{self, DesignScene},
};

fn golden_path(scene: DesignScene, size: (u16, u16)) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/design")
        .join(format!("{}-{}x{}.txt", scene.name(), size.0, size.1))
}

fn render(scene: DesignScene, size: (u16, u16)) -> Vec<String> {
    design::render(&scene.scene(), size).plain()
}

#[test]
fn every_design_scene_matches_its_golden_at_every_size() {
    let update = std::env::var_os("QQ_UPDATE_GOLDENS").is_some();
    let mut mismatches = Vec::new();
    for scene in DesignScene::ALL {
        for &size in GOLDEN_SIZES {
            let path = golden_path(scene, size);
            let mut actual = render(scene, size).join("\n");
            actual.push('\n');
            if update {
                fs::create_dir_all(path.parent().expect("design directory"))
                    .expect("create design directory");
                fs::write(&path, &actual).expect("write design golden");
                continue;
            }
            match fs::read_to_string(&path) {
                Ok(expected) if expected == actual => {}
                Ok(expected) => mismatches.push(format!(
                    "{}: differs\n--- expected\n{expected}\n--- actual\n{actual}",
                    path.display()
                )),
                Err(error) => mismatches.push(format!("{}: {error}", path.display())),
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} design frame(s) differ; rerun with QQ_UPDATE_GOLDENS=1 if intended\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

#[test]
fn every_design_frame_fills_the_terminal_exactly() {
    for scene in DesignScene::ALL {
        for &(width, height) in GOLDEN_SIZES {
            let rows = render(scene, (width, height));
            assert_eq!(
                rows.len(),
                usize::from(height),
                "{scene:?} {width}x{height}"
            );
            for row in &rows {
                assert!(
                    unicode_columns(row) <= usize::from(width),
                    "{scene:?} {width}x{height} overflows: {row:?}"
                );
            }
        }
    }
}

#[test]
fn the_design_has_no_header_label_or_permanent_hint_strip() {
    for scene in DesignScene::ALL {
        for &size in GOLDEN_SIZES {
            let rows = render(scene, size);
            for row in &rows {
                for banned in ["▌ YOU", "^K commands", "ttft 0.0s", "Alt-R", "› Ask"] {
                    assert!(!row.contains(banned), "{scene:?} {size:?} shows {banned:?}");
                }
            }
        }
    }
}

#[test]
fn the_golden_path_gives_the_transcript_more_rows_than_today() {
    let size = (120, 40);
    let today = BenchHarness::scene(Scene::GoldenPath, size).plain_frame();
    let today_body = today.len() - 1 - 3;
    let design_body = design::render(&DesignScene::GoldenPath.scene(), size).body;
    assert!(
        design_body > today_body,
        "design body {design_body} rows, today {today_body}"
    );
}

#[test]
fn the_frame_spans_the_whole_terminal_at_every_size() {
    for scene in DesignScene::ALL {
        for &(width, height) in GOLDEN_SIZES {
            let frame = design::render(&scene.scene(), (width, height));
            let width = usize::from(width);
            let rows = frame.plain();
            let rule = &rows[frame.body];
            let pane = width - frame.sidebar;
            let reach = unicode_columns(rule.trim_end());
            assert!(
                reach + 2 >= pane - 2,
                "{scene:?} {width}: rule stops at {reach} of {pane}"
            );
            if frame.sidebar > 0 {
                for row in &frame.rows {
                    let columns: usize = row.iter().map(|span| unicode_columns(&span.text)).sum();
                    assert_eq!(columns, width, "{scene:?} {width}: sidebar not flush right");
                }
            }
        }
    }
}

fn unicode_columns(row: &str) -> usize {
    row.chars().count()
}
