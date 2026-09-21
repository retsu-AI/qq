//! Responsive frame geometry.
//!
//! The terminal's size selects a [`Tier`], and the tier selects which panes
//! are on screen and how wide each is. Width selects layout, never features:
//! every action is reachable at every size; a wider terminal shows more at
//! once. Height selects density (composer rows), never structure.
//!
//! `compute_layout` is a pure function of `(size, prefs, session count)` and
//! runs once per frame; every pane then paints inside its own [`Rect`]. Panes
//! the current slice does not yet fill (the inspector, extra transcript
//! slots) are reserved here so later slices change only the painters.

/// Terminal columns at which the layout gains a sessions rail beside the
/// transcript rather than a one-row strip beneath it.
pub(crate) const REGULAR_MIN_WIDTH: usize = 90;
/// Columns at which the layout gains an inspector pane for tool detail.
pub(crate) const WIDE_MIN_WIDTH: usize = 160;
/// Columns at which the layout shows more than one transcript side by side.
pub(crate) const ULTRA_MIN_WIDTH: usize = 240;

/// Narrowest usable rail; below this the strip is used instead.
pub(crate) const RAIL_MIN_WIDTH: usize = 20;
/// Widest the rail grows; the rest goes to the transcript.
pub(crate) const RAIL_MAX_WIDTH: usize = 28;
/// The inspector takes this share of the width past the transcript's
/// measure, bounded so tool output stays readable without starving prose.
pub(crate) const INSPECTOR_MIN_WIDTH: usize = 40;
pub(crate) const INSPECTOR_MAX_WIDTH: usize = 80;

/// Widest a transcript pane lays prose out. Wider panes center the measure;
/// the width-keyed caches see one width across every pane at least this wide.
pub(crate) const TRANSCRIPT_MEASURE: usize = 100;

/// Rows the composer may grow to before it scrolls around the caret, by
/// height: a short terminal gives fewer rows to input so the transcript
/// keeps its context.
pub(crate) const MAX_COMPOSER_ROWS_COMPACT: usize = 4;
pub(crate) const MAX_COMPOSER_ROWS: usize = 8;
/// Below this many rows the composer is compact regardless of width.
pub(crate) const SHORT_HEIGHT: usize = 30;

/// Terminals below this cannot hold the chrome and a usable body.
pub(crate) const MIN_WIDTH: usize = 32;
pub(crate) const MIN_HEIGHT: usize = 9;

/// Which layout family the terminal's width selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tier {
    /// One column; sessions as a strip; below [`REGULAR_MIN_WIDTH`].
    Compact,
    /// Transcript plus a sessions rail.
    Regular,
    /// Rail, transcript, and an inspector for tool detail.
    Wide,
    /// Rail, two or three transcripts, and an inspector.
    Ultra,
}

impl Tier {
    #[must_use]
    pub(crate) const fn of(width: usize) -> Self {
        if width >= ULTRA_MIN_WIDTH {
            Self::Ultra
        } else if width >= WIDE_MIN_WIDTH {
            Self::Wide
        } else if width >= REGULAR_MIN_WIDTH {
            Self::Regular
        } else {
            Self::Compact
        }
    }
}

/// How a user wants an optional pane shown. `Auto` follows the tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PanePref {
    #[default]
    Auto,
    Shown,
    Hidden,
}

impl PanePref {
    /// Cycle as a toggle command does: `Auto` and `Shown` hide, `Hidden`
    /// shows. From `Auto` the first press always hides, which is what a user
    /// reaching for the toggle wants when the pane is in the way; it never
    /// depends on knowing the terminal width.
    #[must_use]
    pub(crate) const fn toggled(self) -> Self {
        match self {
            Self::Auto | Self::Shown => Self::Hidden,
            Self::Hidden => Self::Shown,
        }
    }
}

/// The user's standing layout choices. Runtime toggles; not persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LayoutPrefs {
    /// The sessions rail beside the transcript.
    pub rail: PanePref,
    /// The inspector pane for tool detail (Wide and up when `Auto`).
    pub inspector: PanePref,
}

/// A rectangle of terminal cells; `x`/`y` are zero-based from the top-left.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    #[must_use]
    pub(crate) const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[cfg(test)]
    const fn right(self) -> usize {
        self.x + self.width
    }
}

/// Where one transcript pane paints: its cells, and the narrower column the
/// prose lays out in when the slot is wider than the measure. The pane's
/// state (what it follows, scroll) is `viewport::TranscriptPane`; this is
/// only geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptSlot {
    pub area: Rect,
    /// Columns from the pane's left edge to where content starts.
    pub inset: usize,
    /// Width content lays out at; `area.width - 2 * inset` or less.
    pub content_width: usize,
}

/// Where everything goes this frame. Rows are absolute; the body region is
/// what remains between the top row and the drafts/composer block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub tier: Tier,
    pub width: usize,
    pub height: usize,
    /// Row 0.
    pub top_row: Rect,
    /// The region between the top row and the drafts; panes divide it.
    pub body: Rect,
    /// The sessions rail on the right edge of the body, when shown.
    pub rail: Option<Rect>,
    /// The inspector between the transcript and the rail, when shown.
    pub inspector: Option<Rect>,
    /// Transcript slots left to right; one until slice L4, never more than
    /// `viewport::MAX_PANES`.
    pub transcripts: Vec<TranscriptSlot>,
    /// Whether the one-row agent strip replaces the rail (Compact with more
    /// than one session and no rail pinned).
    pub strip: bool,
    /// Rows the composer may grow to at this size.
    pub max_composer_rows: usize,
}

impl Layout {
    /// Whether the rail is on screen.
    #[must_use]
    pub(crate) const fn rail_visible(&self) -> bool {
        self.rail.is_some()
    }
}

/// Fixed rows that never belong to the body: the top row and the composer
/// rule. The composer's own rows are counted by the caller after it is laid
/// out, because it grows with wrapped input.
pub(crate) const FIXED_CHROME_ROWS: usize = 2;

/// Rows the composer may grow to at this size. Known before the layout
/// because the composer is laid out first: its row count is part of the
/// chrome the body must leave room for.
#[must_use]
pub(crate) const fn max_composer_rows(width: usize, height: usize) -> usize {
    if height < SHORT_HEIGHT || matches!(Tier::of(width), Tier::Compact) {
        MAX_COMPOSER_ROWS_COMPACT
    } else {
        MAX_COMPOSER_ROWS
    }
}

/// Lay out a frame of `width` × `height` cells. `chrome_rows` are the rows
/// below the body already claimed by drafts, the rule, and the composer; the
/// layout subtracts the agent strip itself when it decides to show one, so
/// the body gets what remains. The caller passes the current session count
/// because `Auto` panes hide when there is nothing to list.
#[must_use]
pub(crate) fn compute_layout(
    width: usize,
    height: usize,
    chrome_rows: usize,
    prefs: LayoutPrefs,
    sessions: usize,
) -> Layout {
    let tier = Tier::of(width);

    let rail_wanted = match prefs.rail {
        PanePref::Auto => tier >= Tier::Regular && sessions > 1,
        PanePref::Shown => true,
        PanePref::Hidden => false,
    };
    // The rail takes a quarter of the terminal within its bounds, but never
    // so much that the transcript drops below the minimum usable width.
    let rail_width = if rail_wanted {
        let quarter = (width / 4).clamp(RAIL_MIN_WIDTH, RAIL_MAX_WIDTH);
        if width.saturating_sub(quarter) >= MIN_WIDTH {
            quarter
        } else {
            0
        }
    } else {
        0
    };
    let strip = rail_width == 0 && sessions > 1;
    let body_height = height
        .saturating_sub(1)
        .saturating_sub(chrome_rows)
        .saturating_sub(usize::from(strip));
    let body = Rect::new(0, 1, width, body_height);

    // `Auto` shows the inspector from Wide once slice L3 paints tool detail
    // in it; until then an empty pane would cost a column of diff per frame
    // for nothing, so `Auto` resolves to hidden and only `Shown` opens it.
    let inspector_wanted = match prefs.inspector {
        PanePref::Auto => false,
        PanePref::Shown => true,
        PanePref::Hidden => false,
    };
    let remaining = width.saturating_sub(rail_width);
    // The inspector is carved from the space past one full measure so the
    // transcript never narrows below its measure to make room for it.
    let inspector_width = if inspector_wanted {
        let spare = remaining.saturating_sub(TRANSCRIPT_MEASURE + 2);
        if spare >= INSPECTOR_MIN_WIDTH {
            spare.min(INSPECTOR_MAX_WIDTH)
        } else {
            0
        }
    } else {
        0
    };
    let transcript_width = remaining.saturating_sub(inspector_width);

    let mut x = 0;
    let transcript_area = Rect::new(x, body.y, transcript_width, body_height);
    x += transcript_width;
    let inspector = (inspector_width > 0).then(|| {
        let rect = Rect::new(x, body.y, inspector_width, body_height);
        x += inspector_width;
        rect
    });
    let rail = (rail_width > 0).then(|| Rect::new(x, body.y, rail_width, body_height));

    Layout {
        tier,
        width,
        height,
        top_row: Rect::new(0, 0, width, 1),
        body,
        rail,
        inspector,
        transcripts: vec![transcript_slot(transcript_area)],
        strip,
        max_composer_rows: max_composer_rows(width, height),
    }
}

/// Place the measure inside `area` when the pane is wider than it. The
/// content sits one third of the way across rather than dead center: the
/// eye starts at the left where the brand and the composer caret are, and a
/// lone pane on an ultra-wide display should read as anchored, not adrift.
/// Until slice L4 fills the remaining width with more panes this keeps the
/// single-transcript frame legible.
fn transcript_slot(area: Rect) -> TranscriptSlot {
    let content_width = area.width.min(TRANSCRIPT_MEASURE);
    let inset = (area.width - content_width) / 3;
    TranscriptSlot {
        area,
        inset,
        content_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(width: usize, height: usize, sessions: usize) -> Layout {
        compute_layout(width, height, 2, LayoutPrefs::default(), sessions)
    }

    #[test]
    fn tiers_change_exactly_at_the_breakpoints() {
        assert_eq!(Tier::of(89), Tier::Compact);
        assert_eq!(Tier::of(90), Tier::Regular);
        assert_eq!(Tier::of(159), Tier::Regular);
        assert_eq!(Tier::of(160), Tier::Wide);
        assert_eq!(Tier::of(239), Tier::Wide);
        assert_eq!(Tier::of(240), Tier::Ultra);
    }

    #[test]
    fn compact_has_one_column_and_a_strip_when_there_are_agents() {
        let one = layout(80, 24, 1);
        assert_eq!(one.tier, Tier::Compact);
        assert!(one.rail.is_none());
        assert!(one.inspector.is_none());
        assert!(!one.strip, "a lone session has nothing to list");
        assert_eq!(one.transcripts[0].area, Rect::new(0, 1, 80, 21));
        assert_eq!(one.transcripts[0].inset, 0);
        assert_eq!(one.transcripts[0].content_width, 80);
        assert_eq!(one.max_composer_rows, MAX_COMPOSER_ROWS_COMPACT);

        let many = layout(80, 24, 3);
        assert!(many.strip);
        assert!(many.rail.is_none());
        assert_eq!(
            many.transcripts[0].area.height, 20,
            "the strip takes one body row"
        );
    }

    #[test]
    fn regular_adds_a_rail_only_when_there_is_more_than_one_session() {
        let one = layout(120, 40, 1);
        assert!(one.rail.is_none());
        assert_eq!(one.transcripts[0].area.width, 120);
        assert_eq!(one.transcripts[0].content_width, TRANSCRIPT_MEASURE);
        assert_eq!(one.transcripts[0].inset, 6, "measure placed a third across");

        let many = layout(120, 40, 3);
        let rail = many.rail.expect("rail");
        assert_eq!(rail, Rect::new(92, 1, 28, 37));
        assert!(!many.strip);
        assert_eq!(many.transcripts[0].area.width, 92);
        assert_eq!(many.transcripts[0].content_width, 92);
        assert!(many.inspector.is_none());
    }

    #[test]
    fn the_rail_never_starves_the_transcript() {
        let prefs = LayoutPrefs {
            rail: PanePref::Shown,
            inspector: PanePref::Auto,
        };
        let tight = compute_layout(60, 24, 2, prefs, 3);
        assert_eq!(tight.rail, Some(Rect::new(40, 1, 20, 21)));
        assert_eq!(tight.transcripts[0].area.width, 40);
        // Narrower still: the rail gives way rather than the transcript,
        // and the strip returns.
        let tiny = compute_layout(50, 24, 2, prefs, 3);
        assert!(tiny.rail.is_none());
        assert!(tiny.strip);
        assert_eq!(tiny.transcripts[0].area.height, 20);
    }

    #[test]
    fn wide_carves_an_inspector_past_the_measure_when_shown() {
        let auto = layout(200, 60, 3);
        assert_eq!(auto.tier, Tier::Wide);
        assert!(
            auto.inspector.is_none(),
            "Auto stays hidden until L3 paints the pane"
        );
        assert_eq!(auto.transcripts[0].area.width, 172);

        let wide = compute_layout(
            200,
            60,
            2,
            LayoutPrefs {
                rail: PanePref::Auto,
                inspector: PanePref::Shown,
            },
            3,
        );
        let rail = wide.rail.expect("rail");
        let inspector = wide.inspector.expect("inspector");
        let transcript = wide.transcripts[0];
        assert_eq!(rail.width, 28);
        // 200 - 28 = 172 remaining; 172 - 102 = 70 spare, within bounds.
        assert_eq!(inspector.width, 70);
        assert_eq!(transcript.area.width, 102);
        assert_eq!(transcript.content_width, TRANSCRIPT_MEASURE);
        // Left to right with no gaps or overlap.
        assert_eq!(transcript.area.x, 0);
        assert_eq!(inspector.x, transcript.area.right());
        assert_eq!(rail.x, inspector.right());
        assert_eq!(rail.right(), 200);
    }

    #[test]
    fn ultra_fills_the_whole_width_and_caps_the_inspector() {
        let ultra = compute_layout(
            480,
            120,
            2,
            LayoutPrefs {
                rail: PanePref::Auto,
                inspector: PanePref::Shown,
            },
            3,
        );
        assert_eq!(ultra.tier, Tier::Ultra);
        let rail = ultra.rail.expect("rail");
        let inspector = ultra.inspector.expect("inspector");
        assert_eq!(inspector.width, INSPECTOR_MAX_WIDTH);
        assert_eq!(rail.right(), 480, "every column is painted");
        let transcript = ultra.transcripts[0];
        assert_eq!(transcript.area.width, 480 - 28 - 80);
        assert_eq!(transcript.content_width, TRANSCRIPT_MEASURE);
        assert_eq!(
            transcript.inset,
            (transcript.area.width - TRANSCRIPT_MEASURE) / 3
        );
        assert_eq!(ultra.max_composer_rows, MAX_COMPOSER_ROWS);
    }

    #[test]
    fn hidden_and_shown_prefs_override_the_tier() {
        let hidden = compute_layout(
            200,
            60,
            2,
            LayoutPrefs {
                rail: PanePref::Hidden,
                inspector: PanePref::Hidden,
            },
            3,
        );
        assert!(hidden.rail.is_none());
        assert!(hidden.inspector.is_none());
        assert!(hidden.strip, "hiding the rail brings the strip back");
        assert_eq!(hidden.transcripts[0].area.width, 200);
        assert_eq!(hidden.transcripts[0].area.height, 60 - 1 - 2 - 1);

        let shown = compute_layout(
            80,
            24,
            2,
            LayoutPrefs {
                rail: PanePref::Shown,
                inspector: PanePref::Auto,
            },
            1,
        );
        assert_eq!(shown.rail, Some(Rect::new(60, 1, 20, 21)));
    }

    #[test]
    fn short_terminals_get_a_compact_composer_at_every_width() {
        assert_eq!(max_composer_rows(200, 24), MAX_COMPOSER_ROWS_COMPACT);
        assert_eq!(max_composer_rows(200, 30), MAX_COMPOSER_ROWS);
        assert_eq!(max_composer_rows(80, 60), MAX_COMPOSER_ROWS_COMPACT);
        assert_eq!(layout(200, 30, 1).max_composer_rows, MAX_COMPOSER_ROWS);
    }

    #[test]
    fn toggling_a_pref_hides_first_then_alternates() {
        assert_eq!(PanePref::Auto.toggled(), PanePref::Hidden);
        assert_eq!(PanePref::Hidden.toggled(), PanePref::Shown);
        assert_eq!(PanePref::Shown.toggled(), PanePref::Hidden);
    }
}
