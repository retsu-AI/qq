pub(crate) const REGULAR_MIN_WIDTH: usize = 90;
pub(crate) const WIDE_MIN_WIDTH: usize = 160;
pub(crate) const ULTRA_MIN_WIDTH: usize = 240;

pub(crate) const RAIL_MIN_WIDTH: usize = 20;
pub(crate) const RAIL_MAX_WIDTH: usize = 28;
pub(crate) const INSPECTOR_MIN_WIDTH: usize = 40;
pub(crate) const INSPECTOR_MAX_WIDTH: usize = 80;

pub(crate) const TRANSCRIPT_MEASURE: usize = 100;

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
    pub top_row: Rect,
    pub body: Rect,
    pub rail: Option<Rect>,
    pub inspector: Option<Rect>,
    pub transcripts: Vec<TranscriptSlot>,
    pub strip: bool,
    pub max_composer_rows: usize,
}

impl Layout {
    #[must_use]
    pub(crate) const fn rail_visible(&self) -> bool {
        self.rail.is_some()
    }
}

pub(crate) const FIXED_CHROME_ROWS: usize = 2;

pub(crate) const COMPOSER_PADDING_MIN_HEIGHT: usize = 20;

#[must_use]
pub(crate) const fn fixed_chrome_rows(height: usize) -> usize {
    FIXED_CHROME_ROWS + composer_padding_rows(height)
}

#[must_use]
pub(crate) const fn composer_padding_rows(height: usize) -> usize {
    if height >= COMPOSER_PADDING_MIN_HEIGHT {
        1
    } else {
        0
    }
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

    let inspector_wanted = match prefs.inspector {
        PanePref::Auto => tier >= Tier::Wide,
        PanePref::Shown => true,
        PanePref::Hidden => false,
    };
    let remaining = width.saturating_sub(rail_width);
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
        let tiny = compute_layout(50, 24, 2, prefs, 3);
        assert!(tiny.rail.is_none());
        assert!(tiny.strip);
        assert_eq!(tiny.transcripts[0].area.height, 20);
    }

    #[test]
    fn wide_carves_an_inspector_past_the_measure() {
        let regular = layout(159, 60, 3);
        assert_eq!(regular.tier, Tier::Regular);
        assert!(regular.inspector.is_none(), "Auto hides below Wide");

        let auto = layout(200, 60, 3);
        assert_eq!(auto.tier, Tier::Wide);
        assert!(auto.inspector.is_some(), "Auto shows at Wide");

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
        assert_eq!(auto, wide, "Auto and Shown agree at Wide");
        let rail = wide.rail.expect("rail");
        let inspector = wide.inspector.expect("inspector");
        let transcript = wide.transcripts[0];
        assert_eq!(rail.width, 28);
        assert_eq!(inspector.width, 70);
        assert_eq!(transcript.area.width, 102);
        assert_eq!(transcript.content_width, TRANSCRIPT_MEASURE);
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
