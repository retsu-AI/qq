//! The active color theme. The renderer paints with eight semantic roles;
//! a theme maps each to a terminal color (see `docs/design/theme.md`).
//!
//! Style helpers in `render.rs` are called hundreds of times per frame, so
//! the active palette is a `Copy` value in a thread-local read by each
//! helper, refreshed from the shared slot once per frame by the renderer.
//! Switching themes bumps a generation the renderer compares to know when
//! to drop every cached layout and repaint every row.

use std::cell::Cell;

use crossterm::style::Color;

/// One resolved theme: a name for the picker and its role colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub palette: Palette,
}

/// The role colors. `Copy` so a frame can snapshot it for free. The first
/// eight are what a theme file declares; the rest are derived from them
/// unless the theme overrides them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub brand: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
    pub surface: Color,
    /// A second surface a step further from the background, for rules and
    /// the composer frame.
    pub surface_alt: Color,
    /// Background of the selected row in pickers and the sidebar.
    pub selection_bg: Color,
    /// Rules and dividers.
    pub border: Color,
    /// Background tint behind added and removed diff lines.
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
    /// Running-state color: spinners and "working" labels.
    pub info: Color,
    /// Syntax roles for highlighted code panels. Derived from the declared
    /// roles unless the theme's `syntax` block overrides them; none of the
    /// defaults ever reads `error`, so code never looks broken. Comment
    /// italic is applied by the style helper, not stored here.
    pub syn_keyword: Color,
    pub syn_function: Color,
    pub syn_type: Color,
    pub syn_string: Color,
    pub syn_constant: Color,
    pub syn_comment: Color,
    pub syn_property: Color,
    pub syn_punctuation: Color,
}

impl Palette {
    /// The compiled `terminal` palette: the terminal's own ANSI colors for
    /// every role but `brand` and `surface`. The fallback when the terminal
    /// does not advertise truecolor, and selectable by name anywhere.
    pub const TERMINAL: Self = Self {
        text: Color::White,
        muted: Color::DarkGrey,
        accent: Color::Cyan,
        brand: Color::Rgb {
            r: 255,
            g: 159,
            b: 67,
        },
        warning: Color::Yellow,
        error: Color::Red,
        success: Color::Green,
        surface: Color::Rgb {
            r: 38,
            g: 40,
            b: 48,
        },
        surface_alt: Color::Rgb {
            r: 48,
            g: 51,
            b: 61,
        },
        selection_bg: Color::Rgb {
            r: 48,
            g: 51,
            b: 61,
        },
        border: Color::DarkGrey,
        diff_add_bg: Color::Rgb {
            r: 28,
            g: 52,
            b: 36,
        },
        diff_del_bg: Color::Rgb {
            r: 60,
            g: 30,
            b: 34,
        },
        info: Color::Cyan,
        syn_keyword: Color::Rgb {
            r: 255,
            g: 159,
            b: 67,
        },
        syn_function: Color::Cyan,
        syn_type: Color::Yellow,
        syn_string: Color::Green,
        // Brand blended toward text; text is a terminal color here, so the
        // blend falls back to brand itself.
        syn_constant: Color::Rgb {
            r: 255,
            g: 159,
            b: 67,
        },
        syn_comment: Color::DarkGrey,
        syn_property: Color::White,
        syn_punctuation: Color::DarkGrey,
    };

    /// Fill the derived roles from the eight declared ones: the selection and
    /// alternate surface lift the surface a step, borders come from muted and
    /// accent, diff tints from success and error at low intensity, info
    /// follows accent, and the syntax roles map keyword → brand, function →
    /// accent, type → warning, string → success, constant → brand softened a
    /// third of the way toward text, comment and punctuation → muted,
    /// property → text. `error` is deliberately absent from the syntax set.
    #[must_use]
    pub fn derive(roles: [Color; 8]) -> Self {
        let [text, muted, accent, brand, warning, error, success, surface] = roles;
        let lift = |color: Color, amount: u8| match color {
            Color::Rgb { r, g, b } => Color::Rgb {
                r: r.saturating_add(amount),
                g: g.saturating_add(amount),
                b: b.saturating_add(amount),
            },
            other => other,
        };
        let tint = |color: Color, base: Color| match (color, base) {
            (
                Color::Rgb { r, g, b },
                Color::Rgb {
                    r: br,
                    g: bg,
                    b: bb,
                },
            ) => Color::Rgb {
                r: ((u16::from(r) + u16::from(br) * 3) / 4) as u8,
                g: ((u16::from(g) + u16::from(bg) * 3) / 4) as u8,
                b: ((u16::from(b) + u16::from(bb) * 3) / 4) as u8,
            },
            (_, base) => base,
        };
        Self {
            text,
            muted,
            accent,
            brand,
            warning,
            error,
            success,
            surface,
            surface_alt: lift(surface, 10),
            selection_bg: lift(surface, 10),
            border: muted,
            diff_add_bg: tint(success, surface),
            diff_del_bg: tint(error, surface),
            info: accent,
            syn_keyword: brand,
            syn_function: accent,
            syn_type: warning,
            syn_string: success,
            syn_constant: soften(brand, text),
            syn_comment: muted,
            syn_property: text,
            syn_punctuation: muted,
        }
    }

    /// Replace any syntax role the theme overrides, leaving the derived
    /// defaults for the rest.
    #[must_use]
    pub fn with_syntax(mut self, overrides: SyntaxOverrides) -> Self {
        let SyntaxOverrides {
            keyword,
            function,
            r#type,
            string,
            constant,
            comment,
            property,
            punctuation,
        } = overrides;
        if let Some(color) = keyword {
            self.syn_keyword = color.into();
        }
        if let Some(color) = function {
            self.syn_function = color.into();
        }
        if let Some(color) = r#type {
            self.syn_type = color.into();
        }
        if let Some(color) = string {
            self.syn_string = color.into();
        }
        if let Some(color) = constant {
            self.syn_constant = color.into();
        }
        if let Some(color) = comment {
            self.syn_comment = color.into();
        }
        if let Some(color) = property {
            self.syn_property = color.into();
        }
        if let Some(color) = punctuation {
            self.syn_punctuation = color.into();
        }
        self
    }
}

/// `color` moved one third of the way toward `toward`: the constant tone
/// keeps the brand hue at reduced intensity. Terminal (non-RGB) colors have
/// no channels to mix, so the color is returned unchanged.
const fn soften(color: Color, toward: Color) -> Color {
    match (color, toward) {
        (
            Color::Rgb { r, g, b },
            Color::Rgb {
                r: tr,
                g: tg,
                b: tb,
            },
        ) => Color::Rgb {
            r: ((r as u16 * 2 + tr as u16) / 3) as u8,
            g: ((g as u16 * 2 + tg as u16) / 3) as u8,
            b: ((b as u16 * 2 + tb as u16) / 3) as u8,
        },
        (color, _) => color,
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::TERMINAL
    }
}

/// A role color as the composition root supplies it, free of any terminal
/// library type. `Palette` converts it to the renderer's color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColor {
    /// The terminal's own palette entry; follows the user's terminal theme.
    White,
    DarkGrey,
    Cyan,
    Yellow,
    Red,
    Green,
    /// A fixed 24-bit color.
    Rgb(u8, u8, u8),
}

impl From<ThemeColor> for Color {
    fn from(color: ThemeColor) -> Self {
        match color {
            ThemeColor::White => Self::White,
            ThemeColor::DarkGrey => Self::DarkGrey,
            ThemeColor::Cyan => Self::Cyan,
            ThemeColor::Yellow => Self::Yellow,
            ThemeColor::Red => Self::Red,
            ThemeColor::Green => Self::Green,
            ThemeColor::Rgb(r, g, b) => Self::Rgb { r, g, b },
        }
    }
}

/// Eight role colors in the order of `docs/design/theme.md`: text, muted,
/// accent, brand, warning, error, success, surface.
pub type ThemeRoles = [ThemeColor; 8];

/// Syntax roles a theme document's optional `syntax` block sets. `None`
/// keeps the default derived from the declared roles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyntaxOverrides {
    pub keyword: Option<ThemeColor>,
    pub function: Option<ThemeColor>,
    pub r#type: Option<ThemeColor>,
    pub string: Option<ThemeColor>,
    pub constant: Option<ThemeColor>,
    pub comment: Option<ThemeColor>,
    pub property: Option<ThemeColor>,
    pub punctuation: Option<ThemeColor>,
}

impl Theme {
    /// The compiled ANSI fallback. The designed default (`ink`) is a theme
    /// document the composition root loads and passes in like any other.
    #[must_use]
    pub fn terminal() -> Self {
        Self {
            name: "terminal".to_owned(),
            palette: Palette::TERMINAL,
        }
    }

    /// A theme from resolved role colors, syntax roles derived.
    #[must_use]
    pub fn from_roles(name: impl Into<String>, roles: ThemeRoles) -> Self {
        Self::from_roles_and_syntax(name, roles, SyntaxOverrides::default())
    }

    /// A theme from resolved role colors plus the document's `syntax`
    /// overrides, applied after the defaults are derived.
    #[must_use]
    pub fn from_roles_and_syntax(
        name: impl Into<String>,
        roles: ThemeRoles,
        syntax: SyntaxOverrides,
    ) -> Self {
        let [text, muted, accent, brand, warning, error, success, surface] = roles;
        Self {
            name: name.into(),
            palette: Palette::derive([
                text.into(),
                muted.into(),
                accent.into(),
                brand.into(),
                warning.into(),
                error.into(),
                success.into(),
                surface.into(),
            ])
            .with_syntax(syntax),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::terminal()
    }
}

thread_local! {
    static ACTIVE: Cell<Palette> = const { Cell::new(Palette::TERMINAL) };
}

/// Install `palette` for style helpers on this thread. The renderer calls
/// this at the top of every frame; tests call it to render under a theme.
pub(crate) fn activate(palette: Palette) {
    ACTIVE.with(|active| active.set(palette));
}

/// The palette style helpers read. One thread-local load, no lock.
pub(crate) fn active() -> Palette {
    ACTIVE.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_palette_is_the_compiled_terminal_look() {
        assert_eq!(Theme::default().name, "terminal");
        assert_eq!(Palette::default(), Palette::TERMINAL);
        assert_eq!(active(), Palette::TERMINAL);
        assert_eq!(
            Palette::TERMINAL.text,
            Color::White,
            "an ANSI palette entry"
        );
    }

    #[test]
    fn activation_is_per_thread_and_repeatable() {
        let custom = Palette {
            accent: Color::Magenta,
            ..Palette::TERMINAL
        };
        activate(custom);
        assert_eq!(active().accent, Color::Magenta);
        std::thread::spawn(|| assert_eq!(active(), Palette::TERMINAL))
            .join()
            .unwrap();
        activate(Palette::TERMINAL);
        assert_eq!(active(), Palette::TERMINAL);
    }

    fn syntax_roles(palette: Palette) -> [Color; 8] {
        [
            palette.syn_keyword,
            palette.syn_function,
            palette.syn_type,
            palette.syn_string,
            palette.syn_constant,
            palette.syn_comment,
            palette.syn_property,
            palette.syn_punctuation,
        ]
    }

    #[test]
    fn derived_syntax_roles_follow_the_declared_roles_and_never_use_error() {
        const fn rgb(r: u8, g: u8, b: u8) -> Color {
            Color::Rgb { r, g, b }
        }
        let text = rgb(0xe0, 0xde, 0xf4);
        let muted = rgb(0x6e, 0x6a, 0x86);
        let accent = rgb(0xc4, 0xa7, 0xe7);
        let brand = rgb(0xeb, 0xbc, 0xba);
        let warning = rgb(0xf6, 0xc1, 0x77);
        let error = rgb(0xeb, 0x6f, 0x92);
        let success = rgb(0x9c, 0xcf, 0xd8);
        let surface = rgb(0x40, 0x3d, 0x52);
        let palette =
            Palette::derive([text, muted, accent, brand, warning, error, success, surface]);

        assert_eq!(palette.syn_keyword, brand);
        assert_eq!(palette.syn_function, accent);
        assert_eq!(palette.syn_type, warning);
        assert_eq!(palette.syn_string, success);
        assert_eq!(palette.syn_comment, muted);
        assert_eq!(palette.syn_property, text);
        assert_eq!(palette.syn_punctuation, muted);
        // Two thirds brand, one third text, per channel.
        assert_eq!(palette.syn_constant, rgb(0xe7, 0xc7, 0xcd));
        assert_ne!(palette.syn_constant, brand, "constant is softened");
        for role in syntax_roles(palette) {
            assert_ne!(role, error, "no syntax role maps to error");
        }

        // The compiled `terminal` palette mixes ANSI colors, which have no
        // channels to blend: constant stays brand, and nothing is red.
        for role in syntax_roles(Palette::TERMINAL) {
            assert_ne!(role, Palette::TERMINAL.error);
        }
        assert_eq!(Palette::TERMINAL.syn_constant, Palette::TERMINAL.brand);
        assert_eq!(
            syntax_roles(Palette::derive([
                Palette::TERMINAL.text,
                Palette::TERMINAL.muted,
                Palette::TERMINAL.accent,
                Palette::TERMINAL.brand,
                Palette::TERMINAL.warning,
                Palette::TERMINAL.error,
                Palette::TERMINAL.success,
                Palette::TERMINAL.surface,
            ])),
            syntax_roles(Palette::TERMINAL),
            "the compiled constant's syntax roles match derivation"
        );

        // A synthetic palette whose brand *is* an error-like red still
        // yields a constant distinct from error because of the blend.
        let hot = Palette::derive([
            text,
            muted,
            accent,
            rgb(0xff, 0x00, 0x00),
            warning,
            rgb(0xff, 0x00, 0x00),
            success,
            surface,
        ]);
        assert_ne!(hot.syn_constant, hot.error);
    }

    #[test]
    fn syntax_overrides_replace_only_the_named_roles() {
        let base = Theme::from_roles(
            "t",
            [
                ThemeColor::Rgb(1, 1, 1),
                ThemeColor::Rgb(2, 2, 2),
                ThemeColor::Rgb(3, 3, 3),
                ThemeColor::Rgb(4, 4, 4),
                ThemeColor::Rgb(5, 5, 5),
                ThemeColor::Rgb(6, 6, 6),
                ThemeColor::Rgb(7, 7, 7),
                ThemeColor::Rgb(8, 8, 8),
            ],
        );
        let overridden = Theme::from_roles_and_syntax(
            "t",
            [
                ThemeColor::Rgb(1, 1, 1),
                ThemeColor::Rgb(2, 2, 2),
                ThemeColor::Rgb(3, 3, 3),
                ThemeColor::Rgb(4, 4, 4),
                ThemeColor::Rgb(5, 5, 5),
                ThemeColor::Rgb(6, 6, 6),
                ThemeColor::Rgb(7, 7, 7),
                ThemeColor::Rgb(8, 8, 8),
            ],
            SyntaxOverrides {
                keyword: Some(ThemeColor::Rgb(0x10, 0x20, 0x30)),
                punctuation: Some(ThemeColor::White),
                ..SyntaxOverrides::default()
            },
        );
        assert_eq!(
            overridden.palette.syn_keyword,
            Color::Rgb {
                r: 0x10,
                g: 0x20,
                b: 0x30
            }
        );
        assert_eq!(overridden.palette.syn_punctuation, Color::White);
        assert_eq!(
            Palette {
                syn_keyword: base.palette.syn_keyword,
                syn_punctuation: base.palette.syn_punctuation,
                ..overridden.palette
            },
            base.palette,
            "every other role is untouched"
        );
        assert_eq!(
            base.palette.with_syntax(SyntaxOverrides::default()),
            base.palette
        );
    }
}
