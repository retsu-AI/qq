pub(crate) const REGULAR_MIN_WIDTH: usize = 90;
pub(crate) const WIDE_MIN_WIDTH: usize = 160;
pub(crate) const ULTRA_MIN_WIDTH: usize = 240;

pub(crate) const RAIL_REGULAR_WIDTH: usize = 32;
pub(crate) const RAIL_WIDE_WIDTH: usize = 42;
pub(crate) const INSPECTOR_MIN_WIDTH: usize = 40;
pub(crate) const INSPECTOR_MAX_WIDTH: usize = 80;
pub(crate) const INSPECTOR_TRANSCRIPT_MIN: usize = 100;

pub(crate) const MARGIN: usize = 2;

pub(crate) const MAX_COMPOSER_ROWS_COMPACT: usize = 4;
pub(crate) const MAX_COMPOSER_ROWS: usize = 8;
pub(crate) const SHORT_HEIGHT: usize = 30;

pub(crate) const MIN_WIDTH: usize = 32;
pub(crate) const MIN_HEIGHT: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tier {
    Compact,
    Regular,
    Wide,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PanePref {
    #[default]
    Auto,
    Shown,
    Hidden,
}

impl PanePref {
    #[must_use]
    pub(crate) const fn toggled(self) -> Self {
        match self {
            Self::Auto | Self::Shown => Self::Hidden,
            Self::Hidden => Self::Shown,
        }
    }

    #[must_use]
    pub(crate) const fn toggled_from_hidden(self) -> Self {
        match self {
            Self::Shown => Self::Hidden,
            Self::Auto | Self::Hidden => Self::Shown,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LayoutPrefs {
    pub rail: PanePref,
    pub inspector: PanePref,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptSlot {
    pub area: Rect,
    pub inset: usize,
    pub content_width: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub tier: Tier,
    pub width: usize,
    pub height: usize,
    pub body: Rect,
    pub pane_width: usize,
    pub rail: Option<Rect>,
    pub inspector: Option<Rect>,
    pub transcripts: Vec<TranscriptSlot>,
    pub max_composer_rows: usize,
}

#[must_use]
pub(crate) const fn max_composer_rows(width: usize, height: usize) -> usize {
    if height < SHORT_HEIGHT || matches!(Tier::of(width), Tier::Compact) {
        MAX_COMPOSER_ROWS_COMPACT
    } else {
        MAX_COMPOSER_ROWS
    }
}

#[must_use]
pub(crate) const fn rail_width(width: usize, prefs: LayoutPrefs, sessions: usize) -> usize {
    let tier = Tier::of(width);
    let wanted = match prefs.rail {
        PanePref::Auto => (tier as u8) >= (Tier::Regular as u8) && sessions > 1,
        PanePref::Shown => true,
        PanePref::Hidden => false,
    };
    if !wanted {
        return 0;
    }
    let rail = if (tier as u8) >= (Tier::Wide as u8) {
        RAIL_WIDE_WIDTH
    } else {
        RAIL_REGULAR_WIDTH
    };
    if width.saturating_sub(rail) >= MIN_WIDTH {
        rail
    } else {
        0
    }
}

#[must_use]
pub(crate) fn compute_layout(
    width: usize,
    height: usize,
    chrome_rows: usize,
    prefs: LayoutPrefs,
    sessions: usize,
) -> Layout {
    let tier = Tier::of(width);
    let rail_width = rail_width(width, prefs, sessions);
    let body_height = height.saturating_sub(chrome_rows);
    let body = Rect::new(0, 0, width, body_height);
    let pane_width = width - rail_width;
    let inspector_width = match prefs.inspector {
        PanePref::Shown => {
            let spare = pane_width.saturating_sub(INSPECTOR_TRANSCRIPT_MIN);
            if spare >= INSPECTOR_MIN_WIDTH {
                spare.min(INSPECTOR_MAX_WIDTH)
            } else {
                0
            }
        }
        PanePref::Auto | PanePref::Hidden => 0,
    };
    let transcript_width = pane_width - inspector_width;
    let transcript_area = Rect::new(0, body.y, transcript_width, body_height);
    let inspector = (inspector_width > 0)
        .then(|| Rect::new(transcript_width, body.y, inspector_width, body_height));
    let rail = (rail_width > 0).then(|| Rect::new(pane_width, 0, rail_width, height));
    Layout {
        tier,
        width,
        height,
        body,
        pane_width,
        rail,
        inspector,
        transcripts: vec![transcript_slot(transcript_area)],
        max_composer_rows: max_composer_rows(width, height),
    }
}

fn transcript_slot(area: Rect) -> TranscriptSlot {
    let inset = if area.width > 4 * MARGIN { MARGIN } else { 0 };
    TranscriptSlot {
        area,
        inset,
        content_width: area.width - 2 * inset,
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
    fn compact_is_one_full_width_pane_with_a_margin() {
        for sessions in [1, 3] {
            let compact = layout(80, 24, sessions);
            assert_eq!(compact.tier, Tier::Compact);
            assert!(compact.rail.is_none());
            assert!(compact.inspector.is_none());
            assert_eq!(compact.transcripts[0].area, Rect::new(0, 0, 80, 22));
            assert_eq!(compact.transcripts[0].inset, MARGIN);
            assert_eq!(compact.transcripts[0].content_width, 76);
        }
        assert_eq!(
            layout(80, 24, 1).max_composer_rows,
            MAX_COMPOSER_ROWS_COMPACT
        );
    }

    #[test]
    fn the_sessions_column_runs_the_full_height_flush_right() {
        let one = layout(120, 40, 1);
        assert!(one.rail.is_none());
        assert_eq!(one.transcripts[0].content_width, 116);

        let regular = layout(120, 40, 3);
        assert_eq!(regular.rail, Some(Rect::new(88, 0, 32, 40)));
        assert_eq!(regular.pane_width, 88);
        assert_eq!(regular.transcripts[0].content_width, 84);

        let wide = layout(200, 60, 3);
        assert_eq!(wide.rail, Some(Rect::new(158, 0, 42, 60)));
        assert!(wide.inspector.is_none(), "Auto never opens the inspector");
    }

    #[test]
    fn the_rail_never_starves_the_transcript() {
        let prefs = LayoutPrefs {
            rail: PanePref::Shown,
            inspector: PanePref::Auto,
        };
        let tight = compute_layout(70, 24, 2, prefs, 3);
        assert_eq!(tight.rail, Some(Rect::new(38, 0, 32, 24)));
        let tiny = compute_layout(60, 24, 2, prefs, 3);
        assert!(tiny.rail.is_none());
    }

    #[test]
    fn a_shown_inspector_takes_the_spare_columns_and_caps_its_width() {
        let prefs = LayoutPrefs {
            rail: PanePref::Auto,
            inspector: PanePref::Shown,
        };
        let wide = compute_layout(200, 60, 2, prefs, 3);
        let inspector = wide.inspector.expect("inspector");
        assert_eq!(inspector.width, 58);
        assert_eq!(inspector.x, wide.transcripts[0].area.right());
        assert_eq!(inspector.right(), wide.pane_width);
        let ultra = compute_layout(480, 120, 2, prefs, 3);
        assert_eq!(
            ultra.inspector.expect("inspector").width,
            INSPECTOR_MAX_WIDTH
        );
        assert!(compute_layout(120, 40, 2, prefs, 3).inspector.is_none());
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
        assert_eq!(hidden.transcripts[0].area.width, 200);
        assert_eq!(hidden.transcripts[0].area.height, 58);
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
        assert_eq!(shown.rail, Some(Rect::new(48, 0, 32, 24)));
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
        assert_eq!(PanePref::Auto.toggled_from_hidden(), PanePref::Shown);
        assert_eq!(PanePref::Shown.toggled_from_hidden(), PanePref::Hidden);
        assert_eq!(PanePref::Hidden.toggled_from_hidden(), PanePref::Shown);
    }
}
