use super::*;
use crate::render::{border, diff_line_style, surface_color};
use unicode_width::UnicodeWidthChar;

fn frame_rows(frame: &[Line]) -> Vec<String> {
    frame
        .iter()
        .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
        .collect()
}

#[test]
fn markdown_rows_remain_within_the_render_width() {
    let lines = markdown_lines("**Streaming** text remains narrow and readable.", 9, false);
    assert!(lines.iter().all(|line| line.width() <= 9));
}

#[test]
fn tables_render_aligned_columns_with_a_header_separator() {
    let source =
        "| Order | Source |\n| --- | --- |\n| 1 | Built-in defaults |\n| 2 | Cached manifest |";
    let lines = markdown_lines(source, 60, false);
    let rows = frame_rows(&lines);

    assert_eq!(
        rows,
        [
            "Order │ Source".to_owned(),
            format!("{}─┼─{}", "─".repeat(5), "─".repeat(17)),
            "1     │ Built-in defaults".to_owned(),
            "2     │ Cached manifest".to_owned(),
        ]
    );
    assert!(lines[0].spans[0].style.bold, "header row renders bold");
}

#[test]
fn wide_tables_wrap_cell_content_within_columns() {
    let source = "| Key | Description |\n| --- | --- |\n| alpha | a very long description that must wrap inside its own column |";
    let width = 32;
    let lines = markdown_lines(source, width, false);
    let rows = frame_rows(&lines);

    assert!(lines.iter().all(|line| line.width() <= width));
    // The oversized description wraps into multiple physical rows.
    assert!(rows.iter().filter(|row| row.contains('│')).count() > 2);
    // Every column separator sits at the same display position.
    let positions = rows
        .iter()
        .filter(|row| row.contains('│') || row.contains('┼'))
        .map(|row| {
            row.chars()
                .take_while(|character| *character != '│' && *character != '┼')
                .map(|character| UnicodeWidthChar::width(character).unwrap_or_default())
                .sum::<usize>()
        })
        .collect::<Vec<_>>();
    assert!(!positions.is_empty());
    assert!(positions.iter().all(|position| *position == positions[0]));
}

#[test]
fn very_narrow_tables_stack_rows_as_header_value_lines() {
    let source = "| A | B | C |\n| --- | --- | --- |\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |";
    let rows = frame_rows(&markdown_lines(source, 10, false));

    assert_eq!(
        rows,
        ["A: 1", "B: 2", "C: 3", "---", "A: 4", "B: 5", "C: 6"]
    );
}

#[test]
fn cjk_table_content_aligns_by_display_width() {
    let source = "| 名前 | 説明 |\n| --- | --- |\n| 短い | 長い説明テキスト |";
    let lines = markdown_lines(source, 40, false);
    let rows = frame_rows(&lines);

    assert!(lines.iter().all(|line| line.width() <= 40));
    let positions = rows
        .iter()
        .map(|row| {
            row.chars()
                .take_while(|character| *character != '│' && *character != '┼')
                .map(|character| UnicodeWidthChar::width(character).unwrap_or_default())
                .sum::<usize>()
        })
        .collect::<Vec<_>>();
    assert_eq!(positions, [5, 5, 5]);
}

#[test]
fn partial_streaming_table_input_never_panics_and_stays_bounded() {
    let fragments = [
        "| Order | Source",
        "| Order | Source |\n| ---",
        "| Order | Source |\n| --- | --- |\n| 1 | Built",
        "| a |\n| --- |\n| b |\n\ntext after",
        "| |\n| --- |\n| |",
    ];
    for fragment in fragments {
        for width in 0..48 {
            let lines = markdown_lines(fragment, width, false);
            assert!(lines.iter().all(|line| line.width() <= width.max(1)));
        }
    }
}

#[test]
fn soft_breaks_reflow_paragraphs_to_the_render_width() {
    // A source-wrapped paragraph joins into one row when it fits...
    assert_eq!(
        frame_rows(&markdown_lines("alpha beta\ngamma delta", 40, false)),
        ["alpha beta gamma delta"]
    );
    // ...and rewraps at the terminal width, not the source width.
    assert_eq!(
        frame_rows(&markdown_lines("alpha beta\ngamma delta", 12, false)),
        ["alpha beta", "gamma delta"]
    );
    // A hard break still forces an explicit line break.
    assert_eq!(
        frame_rows(&markdown_lines("alpha  \nbeta", 40, false)),
        ["alpha", "beta"]
    );
}

#[test]
fn code_blocks_keep_character_wrapping() {
    let rows = frame_rows(&markdown_lines(
        "```\nlet answer_value = 42;\n```",
        12,
        false,
    ));

    assert_eq!(
        rows,
        [
            "│           ",
            "│ let answer",
            "│ _value = 4",
            "│ 2;        ",
        ]
    );
}

#[test]
fn fenced_code_renders_as_a_tinted_panel_with_a_language_label() {
    let width = 24;
    let lines = markdown_lines("```rust\nlet x = 1;\n```", width, false);
    let rows = frame_rows(&lines);

    assert_eq!(
        rows,
        [
            format!("│ rust{}", " ".repeat(18)),
            format!("│ let x = 1;{}", " ".repeat(12)),
        ]
    );
    // Every row is padded to the full width with the surface tint so the
    // panel reads as one solid slab.
    assert!(lines.iter().all(|line| line.width() == width));
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.background == Some(surface_color()))
    );
    assert_eq!(lines[0].spans[0].style, surface(accent().dim()));
    assert_eq!(lines[0].spans[1].style, surface(muted()));
}

#[test]
fn diff_fenced_blocks_color_lines_inside_the_panel() {
    let source = "```diff\n@@ -1,2 +1,2 @@\n-old line\n+new line\n context\n```";
    let lines = markdown_lines(source, 30, false);

    let style_of = |needle: &str| {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(needle))
            .map(|span| span.style)
    };
    assert_eq!(style_of("@@ -1,2 +1,2 @@"), Some(surface(muted())));
    assert_eq!(style_of("-old line"), Some(surface(diff_line_style("-"))));
    assert_eq!(style_of("+new line"), Some(surface(diff_line_style("+"))));
    assert_eq!(style_of(" context"), Some(surface(normal())));
    assert!(lines.iter().all(|line| line.width() == 30));
}

#[test]
fn unterminated_fences_render_panels_safely_mid_stream() {
    let fragments = [
        "```",
        "```rust",
        "```rust\nfn main() {",
        "prose\n\n```diff\n+partial",
    ];
    for fragment in fragments {
        for width in 0..48 {
            for highlight in [false, true] {
                let lines = markdown_lines(fragment, width, highlight);
                assert!(lines.iter().all(|line| line.width() <= width.max(1)));
            }
        }
    }
    // A fence still streaming renders as a panel with the text so far.
    let rows = frame_rows(&markdown_lines("```rust\nfn main() {", 24, false));
    assert!(rows.iter().any(|row| row.starts_with("│ fn main() {")));
}

/// Finds the style of the first span whose text contains `needle`.
pub(crate) fn style_of(lines: &[Line], needle: &str) -> Option<Style> {
    lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| span.text.contains(needle))
        .map(|span| span.style)
}

#[test]
fn fence_tags_map_to_bundled_grammars_including_aliases() {
    for tag in [
        "rust",
        "toml",
        "json",
        "yaml",
        "bash",
        "python",
        "javascript",
        "typescript",
        "tsx",
        "jsx",
        "go",
        "c",
        "cpp",
    ] {
        assert!(
            fence_highlight_configuration(tag).is_some(),
            "{tag} maps to a grammar with a compiling query"
        );
    }
    for (alias, canonical) in [
        ("rs", "rust"),
        ("Rust", "rust"),
        ("sh", "bash"),
        ("shell", "bash"),
        ("zsh", "bash"),
        ("py", "python"),
        ("js", "javascript"),
        ("ts", "typescript"),
        ("yml", "yaml"),
        ("golang", "go"),
        ("c++", "cpp"),
        ("jsonc", "json"),
    ] {
        let (alias_configuration, canonical_configuration) = (
            fence_highlight_configuration(alias).expect("alias resolves"),
            fence_highlight_configuration(canonical).expect("canonical resolves"),
        );
        assert!(
            std::ptr::eq(alias_configuration, canonical_configuration),
            "{alias} shares {canonical}'s configuration"
        );
    }
    // Unknown tags render plain; diff keeps its dedicated coloring and
    // ron has no maintained grammar crate.
    for tag in ["", "diff", "ron", "console", "brainfuck"] {
        assert!(fence_highlight_configuration(tag).is_none(), "{tag} plain");
    }
}

#[test]
fn highlighted_rust_panels_style_keywords_strings_and_comments() {
    let source = "```rust\n// note\nlet x = \"hi\";\n```";
    let width = 40;
    let lines = markdown_lines(source, width, true);

    assert_eq!(style_of(&lines, "// note"), Some(surface(code_comment())));
    assert_eq!(style_of(&lines, "let"), Some(surface(code_keyword())));
    assert_eq!(style_of(&lines, "\"hi\""), Some(surface(code_string())));
    // Every highlighted span still carries the panel tint, every row pads
    // to the full width, and the panel structure (label row, gutter,
    // padding rows) matches the plain rendering exactly.
    assert!(lines.iter().all(|line| line.width() == width));
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.background == Some(surface_color()))
    );
    assert_eq!(
        frame_rows(&lines),
        frame_rows(&markdown_lines(source, width, false))
    );
    assert_eq!(lines[0].spans[0].style, surface(accent().dim()));
    assert_eq!(lines[0].spans[1].style, surface(muted()));
}

#[test]
fn highlighted_long_lines_keep_character_wrapping_and_span_styles() {
    let source = "```rust\nlet answer = \"abcdefghijklmnopqrst\";\n```";
    let width = 16;
    let lines = markdown_lines(source, width, true);

    assert!(lines.iter().all(|line| line.width() == width));
    // Character wrapping, not reflow: the rows match the plain panel.
    assert_eq!(
        frame_rows(&lines),
        frame_rows(&markdown_lines(source, width, false))
    );
    // The wrapped string literal keeps its style on every row it spans.
    let string_rows = lines
        .iter()
        .filter(|line| {
            line.spans
                .iter()
                .any(|span| span.style == surface(code_string()))
        })
        .count();
    assert!(string_rows >= 2, "string literal spans wrapped rows");
}

#[test]
fn oversized_code_blocks_fall_back_to_plain_panel_text() {
    let source = format!(
        "```rust\nlet x = 1;\n{}```",
        "// pad\n".repeat(MAX_HIGHLIGHT_BYTES / 7 + 1)
    );
    let lines = markdown_lines(&source, 40, true);

    assert_eq!(style_of(&lines, "let"), Some(surface(normal())));
    assert_eq!(style_of(&lines, "// pad"), Some(surface(normal())));
}

#[test]
fn diff_fences_keep_diff_coloring_when_highlighting_is_enabled() {
    let source = "```diff\n@@ -1 +1 @@\n-old line\n+new line\n```";
    let lines = markdown_lines(source, 30, true);

    assert_eq!(style_of(&lines, "@@ -1 +1 @@"), Some(surface(muted())));
    assert_eq!(
        style_of(&lines, "-old line"),
        Some(surface(diff_line_style("-")))
    );
    assert_eq!(
        style_of(&lines, "+new line"),
        Some(surface(diff_line_style("+")))
    );
}

#[test]
fn headings_get_a_blank_line_above_and_lists_stay_tight() {
    let rows = frame_rows(&markdown_lines(
        "intro\n# Title\n- alpha\n- beta",
        40,
        false,
    ));

    assert_eq!(
        rows,
        ["intro", "", "Title", "─────", "", "• alpha", "• beta"]
    );
}

#[test]
fn markdown_entities_cannot_emit_terminal_controls() {
    let lines = markdown_lines("&#27;]52;c;Y2xpcGJvYXJk&#7;", 80, false);
    assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
        span.text
            .chars()
            .all(|character| terminal_safe_character(character) == Some(character))
    }));
}

mod settled_prefix_tests {
    use super::*;

    /// Render the prefix and suffix independently and concatenate them the
    /// way the live renderer does.
    fn split_render(source: &str, width: usize) -> Vec<Line> {
        let split = settled_prefix_end(source);
        let mut lines = markdown_lines(&source[..split], width, false);
        lines.extend(markdown_lines(&source[split..], width, false));
        lines
    }

    const CORPUS: &[&str] = &[
        "plain paragraph without a boundary",
        "first paragraph\n\nsecond paragraph",
        "first paragraph\n\nsecond paragraph\n\nthird",
        "# Heading\n\ntext\n\n- item one\n- item two\n\n```rust\nfn main() {}\n```\n\ntail",
        "```rust\nlet x = 1;\n\nlet y = 2;\n```\n\nafter",
        "```\nunterminated\n\nstill code",
        "~~~\ntilde fence\n\n~~~\n\nafter tilde",
        "````\n```\nnested\n```\n````\n\nafter nested",
        "| a | b |\n| - | - |\n| 1 | 2 |\n\nafter table",
        "> quoted\n> more\n\nafter quote",
        "    indented code\n\n    still code\n\nafter",
        "1. one\n2. two\n\n   continued\n\nafter list",
        "trailing blank\n\n",
        "\n\nleading blank",
        "a\n\n\n\nb",
        "text **bold\n\nstill open",
    ];

    #[test]
    fn splitting_at_the_settled_prefix_does_not_change_layout() {
        for source in CORPUS {
            for width in [12, 40, 80] {
                let whole = markdown_lines(source, width, false);
                let split = split_render(source, width);
                assert_eq!(split, whole, "source={source:?} width={width}");
            }
        }
    }

    #[test]
    fn settled_prefix_never_splits_inside_a_fence() {
        let source = "```\ncode\n\nmore code\n";
        assert_eq!(settled_prefix_end(source), 0);
        let source = "before\n\n```\ncode\n\nmore\n```\n\nafter";
        assert_eq!(
            &source[..settled_prefix_end(source)],
            "before\n\n```\ncode\n\nmore\n```\n\n"
        );
    }

    #[test]
    fn settled_prefix_grows_monotonically_as_text_streams() {
        let full =
            "# Heading\n\npara one\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\npara two\n\nlast";
        let mut previous = 0;
        for end in 0..=full.len() {
            if !full.is_char_boundary(end) {
                continue;
            }
            let settled = settled_prefix_end(&full[..end]);
            assert!(
                settled >= previous,
                "end={end} settled={settled} previous={previous}"
            );
            assert!(settled <= end);
            previous = settled;
        }
    }
}

/// Every block element the transcript lays out, in one message.
const GALLERY: &str = "\
First paragraph of prose.

Second paragraph of prose, directly after the first.

# Level one heading

## Level two heading

### Level three heading

1. First numbered item
2. Second numbered item that is deliberately long enough to wrap onto a second row at this width
3. Third numbered item
   - nested bullet under three

- A bullet item that is also deliberately long enough to wrap onto a second physical row here
- Short bullet

- [ ] open task
- [x] done task

> A quote long enough to wrap onto a second row so we can see whether the rail repeats.

Some *emphasis*, some **strong**, some `inline code`, a [link](https://example.com/x), a footnote[^1], and math $x^2$.

[^1]: The footnote body.

```rust
fn main() {
    let x = 1;
}
```

| Role | Default |
| --- | --- |
| text | white |

---

Tail paragraph.
";

#[test]
fn ordered_lists_keep_their_numbers_right_aligned() {
    let rows = frame_rows(&markdown_lines(
        "1. one\n2. two\n3. three\n4. four\n5. five\n6. six\n7. seven\n8. eight\n9. nine\n10. ten",
        40,
        false,
    ));
    assert_eq!(rows[0], " 1. one");
    assert_eq!(rows[8], " 9. nine");
    assert_eq!(rows[9], "10. ten");
    let lines = markdown_lines("1. one", 40, false);
    assert_eq!(style_of(&lines, "1."), Some(accent()));
}

#[test]
fn wrapped_list_items_hang_under_their_text() {
    let rows = frame_rows(&markdown_lines(
        "1. alpha beta gamma delta epsilon\n\n- alpha beta gamma delta epsilon\n  - zeta eta theta iota kappa",
        18,
        false,
    ));
    assert_eq!(rows[0], "1. alpha beta");
    assert_eq!(rows[1], "   gamma delta");
    assert_eq!(rows[2], "   epsilon");
    assert_eq!(rows[3], "");
    assert_eq!(rows[4], "• alpha beta gamma");
    assert_eq!(rows[5], "  delta epsilon");
    assert_eq!(rows[6], "  ◦ zeta eta theta");
    assert_eq!(rows[7], "    iota kappa");
}

#[test]
fn task_items_use_box_glyphs_without_a_bullet() {
    let rows = frame_rows(&markdown_lines("- [ ] open\n- [x] done", 40, false));
    assert_eq!(rows, ["☐ open", "☑ done"]);
    let lines = markdown_lines("- [x] done", 40, false);
    assert_eq!(style_of(&lines, "☑"), Some(accent()));
}

#[test]
fn quotes_carry_the_rail_on_every_wrapped_row() {
    let lines = markdown_lines("> alpha beta gamma delta epsilon zeta", 14, false);
    let rows = frame_rows(&lines);
    assert!(rows.len() >= 3, "{rows:?}");
    for row in &rows {
        assert!(row.starts_with("▎ "), "{row:?}");
        assert!(row.chars().count() <= 14, "{row:?}");
    }
    assert_eq!(style_of(&lines, "▎"), Some(muted()));
    assert_eq!(style_of(&lines, "alpha"), Some(muted().italic()));
}

#[test]
fn rules_span_the_content_width_in_the_border_style() {
    let lines = markdown_lines("above\n\n---\n\nbelow", 24, false);
    let rows = frame_rows(&lines);
    let rule = "─".repeat(24);
    assert_eq!(rows, ["above", "", rule.as_str(), "", "below"]);
    assert_eq!(style_of(&lines, "──"), Some(border()));
}

#[test]
fn heading_levels_differ_and_h1_is_underlined() {
    let lines = markdown_lines("# One\n\n## Two\n\n### Three\n\ntext", 40, false);
    let rows = frame_rows(&lines);
    assert_eq!(rows, ["One", "───", "", "Two", "", "Three", "", "text"]);
    assert_eq!(style_of(&lines, "One"), Some(normal().bold()));
    assert_eq!(style_of(&lines, "───"), Some(border()));
    assert_eq!(style_of(&lines, "Two"), Some(accent().bold()));
    assert_eq!(style_of(&lines, "Three"), Some(normal().bold()));
}

#[test]
fn exactly_one_blank_row_separates_every_block_in_the_gallery() {
    let rows = frame_rows(&markdown_lines(GALLERY, 80, false));
    assert!(
        !rows.first().is_some_and(String::is_empty),
        "no leading gap"
    );
    assert!(
        !rows.last().is_some_and(String::is_empty),
        "no trailing gap"
    );
    for pair in rows.windows(2) {
        assert!(
            !(pair[0].is_empty() && pair[1].is_empty()),
            "two consecutive blank rows in {rows:#?}"
        );
    }
    // Each of these begins a block whose predecessor is a different block,
    // so the row before it must be blank.
    for needle in [
        "Second paragraph",
        "Level one heading",
        "Level two heading",
        "Level three heading",
        "1. First numbered",
        "• A bullet item",
        "▎ A quote",
        "Some emphasis",
        "The footnote body",
        "│ rust",
        "Role",
        "Tail paragraph",
    ] {
        let at = rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing from {rows:#?}"));
        assert!(
            at > 0 && rows[at - 1].is_empty(),
            "{needle:?} has no gap above: {rows:#?}"
        );
    }
    // The horizontal rule spans the width; the H1 underline (title width)
    // sits directly under its heading and is not a block of its own.
    let rule = rows
        .iter()
        .position(|r| *r == "─".repeat(80))
        .expect("full-width rule");
    assert!(
        rows[rule - 1].is_empty() && rows[rule + 1].is_empty(),
        "{rows:#?}"
    );
    let h1 = rows
        .iter()
        .position(|r| r.contains("Level one heading"))
        .unwrap();
    assert_eq!(rows[h1 + 1], "─".repeat("Level one heading".len()));
    // Rows inside one list and one table stay tight. `- Short bullet` and
    // `- [ ] open task` are one CommonMark list despite the blank line
    // between them, so the task items follow the bullets directly.
    let second = rows.iter().position(|r| r.contains("2. Second")).unwrap();
    assert!(!rows[second - 1].is_empty(), "list items stay tight");
    let open_task = rows.iter().position(|r| r.contains("☐ open task")).unwrap();
    assert_eq!(rows[open_task - 1], "• Short bullet");
    let text_row = rows.iter().position(|r| r.contains("text")).unwrap();
    assert!(!rows[text_row - 1].is_empty(), "table rows stay tight");
    // The whole gallery renders identically whether laid out at once or
    // through the settled prefix used while streaming.
    let split = settled_prefix_end(GALLERY);
    let mut streamed = markdown_lines(&GALLERY[..split], 80, false);
    streamed.extend(markdown_lines(&GALLERY[split..], 80, false));
    let streamed = frame_rows(&streamed);
    assert!(
        rows.starts_with(&streamed[..streamed.len().min(8)]),
        "streaming prefix layout diverges"
    );
}
