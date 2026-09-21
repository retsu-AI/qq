//! Gallery dump: every review scene at every golden size under every
//! compiled theme, as the exact bytes a terminal receives, for viewing with
//! `cat` in a real terminal. Ignored by default because it writes files:
//!
//! ```sh
//! cargo test -p qq-tui --test gallery -- --ignored
//! cat target/qq-tui-gallery/ink/markdown-gallery-120x40.ans
//! ```
//!
//! Highlighting lands off-tick, so frames are drawn under a Tokio runtime
//! and settled before they are written.

use std::{fs, path::PathBuf};

use qq_config::{ConfigLoader, ConfigPaths};
use qq_tui::{
    SyntaxOverrides, Theme, ThemeColor, TuiOptions,
    bench_support::{BenchHarness, GOLDEN_SIZES, Scene},
};

fn gallery_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/qq-tui-gallery")
}

/// The compiled themes as the TUI sees them, `qq` first.
fn compiled_themes() -> Vec<Theme> {
    let scratch = tempfile::tempdir().expect("scratch directory for theme discovery");
    let paths = ConfigPaths::new(
        scratch.path().join("global"),
        scratch.path().join("data"),
        scratch.path().join("managed"),
    );
    let loader = ConfigLoader::new(paths);
    let documents = loader
        .discover_themes(scratch.path())
        .expect("compiled themes parse");
    let color = |color: qq_config::ThemeColor| match color {
        qq_config::ThemeColor::Rgb(qq_config::Rgb { r, g, b }) => ThemeColor::Rgb(r, g, b),
        qq_config::ThemeColor::Ansi(ansi) => match ansi {
            qq_config::AnsiColor::White => ThemeColor::White,
            qq_config::AnsiColor::DarkGrey => ThemeColor::DarkGrey,
            qq_config::AnsiColor::Cyan => ThemeColor::Cyan,
            qq_config::AnsiColor::Yellow => ThemeColor::Yellow,
            qq_config::AnsiColor::Red => ThemeColor::Red,
            qq_config::AnsiColor::Green => ThemeColor::Green,
        },
    };
    let mut themes: Vec<Theme> = documents
        .iter()
        .map(|document| {
            let colors = document.colors();
            let syntax = document.syntax();
            Theme::from_roles_and_syntax(
                document.name(),
                [
                    color(colors.text),
                    color(colors.muted),
                    color(colors.accent),
                    color(colors.brand),
                    color(colors.warning),
                    color(colors.error),
                    color(colors.success),
                    color(colors.surface),
                ],
                SyntaxOverrides {
                    keyword: syntax.keyword.map(color),
                    function: syntax.function.map(color),
                    r#type: syntax.r#type.map(color),
                    string: syntax.string.map(color),
                    constant: syntax.constant.map(color),
                    comment: syntax.comment.map(color),
                    property: syntax.property.map(color),
                    punctuation: syntax.punctuation.map(color),
                },
            )
        })
        .collect();
    themes.sort_by(|a, b| {
        (a.name != "qq")
            .cmp(&(b.name != "qq"))
            .then_with(|| a.name.cmp(&b.name))
    });
    themes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "writes target/qq-tui-gallery; run on demand for visual review"]
async fn write_every_scene_under_every_theme() {
    let themes = compiled_themes();
    assert!(themes.len() >= 2, "expected the compiled theme set");
    let root = gallery_root();
    let mut written = 0usize;
    for theme in &themes {
        let directory = root.join(&theme.name);
        fs::create_dir_all(&directory).expect("gallery theme directory");
        for scene in Scene::ALL {
            for &size in GOLDEN_SIZES {
                let options = TuiOptions {
                    themes: vec![theme.clone()],
                    ..TuiOptions::default()
                };
                let mut harness = BenchHarness::scene_with_options(scene, size, options);
                harness.draw_full();
                harness.settle_highlights();
                let frame = harness.ansi_frame();
                let path = directory.join(format!("{}-{}x{}.ans", scene.name(), size.0, size.1));
                fs::write(&path, frame).expect("write gallery frame");
                written += 1;
            }
        }
    }
    assert!(
        written >= Scene::ALL.len() * GOLDEN_SIZES.len(),
        "wrote {written} frames"
    );
    eprintln!("wrote {written} frames under {}", root.display());
}
