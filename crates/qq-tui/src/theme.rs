use std::cell::Cell;

use crossterm::style::Color;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub palette: Palette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    pub brand: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
    pub surface: Color,
    pub surface_alt: Color,
    pub selection_bg: Color,
    pub border: Color,
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
    pub info: Color,
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
    pub const TERMINAL: Self = Self {
        text: Color::White,
        muted: Color::DarkGrey,
        faint: Color::DarkGrey,
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
        syn_constant: Color::Rgb {
            r: 255,
            g: 159,
            b: 67,
        },
        syn_comment: Color::DarkGrey,
        syn_property: Color::White,
        syn_punctuation: Color::DarkGrey,
    };

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
            faint: blend_halfway(muted, surface),
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

#[must_use]
pub const fn blend_halfway(color: Color, background: Color) -> Color {
    match (color, background) {
        (
            Color::Rgb { r, g, b },
            Color::Rgb {
                r: br,
                g: bg,
                b: bb,
            },
        ) => Color::Rgb {
            r: ((r as u16 + br as u16 / 2) / 2) as u8,
            g: ((g as u16 + bg as u16 / 2) / 2) as u8,
            b: ((b as u16 + bb as u16 / 2) / 2) as u8,
        },
        (color, _) => color,
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColor {
    White,
    DarkGrey,
    Cyan,
    Yellow,
    Red,
    Green,
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

pub type ThemeRoles = [ThemeColor; 8];

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
    #[must_use]
    pub fn terminal() -> Self {
        Self {
            name: "terminal".to_owned(),
            palette: Palette::TERMINAL,
        }
    }

    #[must_use]
    pub fn from_roles(name: impl Into<String>, roles: ThemeRoles) -> Self {
        Self::from_roles_and_syntax(name, roles, SyntaxOverrides::default())
    }

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

pub(crate) fn activate(palette: Palette) {
    ACTIVE.with(|active| active.set(palette));
}

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
        assert_eq!(palette.syn_constant, rgb(0xe7, 0xc7, 0xcd));
        assert_ne!(palette.syn_constant, brand, "constant is softened");
        for role in syntax_roles(palette) {
            assert_ne!(role, error, "no syntax role maps to error");
        }

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
