use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

use ron::{Options, extensions::Extensions};
use serde::Deserialize;

use super::{
    ConfigError, ConfigLoader, SourceIdentity, SourceKind,
    loader::{
        Probes, canonical_working_directory, discover_file, project_directories, read_candidate,
    },
};

pub const DEFAULT_THEME: &str = "qq";

pub const TRUECOLOR_DEFAULT_THEME: &str = "ink";

pub const TERMINAL_THEME: &str = "terminal";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruecolorSupport {
    Advertised,
    NotAdvertised,
}

#[must_use]
pub const fn default_theme_name(truecolor: TruecolorSupport) -> &'static str {
    match truecolor {
        TruecolorSupport::Advertised => TRUECOLOR_DEFAULT_THEME,
        TruecolorSupport::NotAdvertised => TERMINAL_THEME,
    }
}

pub const COMPILED_THEMES: &[(&str, &str)] = &[
    ("catppuccin", include_str!("../themes/catppuccin.ron")),
    ("dracula", include_str!("../themes/dracula.ron")),
    ("ember", include_str!("../themes/ember.ron")),
    ("everforest", include_str!("../themes/everforest.ron")),
    ("gruvbox", include_str!("../themes/gruvbox.ron")),
    ("ink", include_str!("../themes/ink.ron")),
    ("kanagawa", include_str!("../themes/kanagawa.ron")),
    ("monokai", include_str!("../themes/monokai.ron")),
    ("nord", include_str!("../themes/nord.ron")),
    ("onedark", include_str!("../themes/onedark.ron")),
    ("rose-pine", include_str!("../themes/rose-pine.ron")),
    ("solarized", include_str!("../themes/solarized.ron")),
    ("tokyonight", include_str!("../themes/tokyonight.ron")),
];

const MAX_DISCOVERED_THEMES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    fn parse(literal: &str) -> Option<Self> {
        let hex = literal.strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
        Some(Self {
            r: channel(0..2)?,
            g: channel(2..4)?,
            b: channel(4..6)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnsiColor {
    White,
    DarkGrey,
    Cyan,
    Yellow,
    Red,
    Green,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThemeColor {
    Ansi(AnsiColor),
    Rgb(Rgb),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeColors {
    pub text: ThemeColor,
    pub muted: ThemeColor,
    pub accent: ThemeColor,
    pub brand: ThemeColor,
    pub warning: ThemeColor,
    pub error: ThemeColor,
    pub success: ThemeColor,
    pub surface: ThemeColor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyntaxRole {
    Keyword,
    Function,
    Type,
    String,
    Constant,
    Comment,
    Property,
    Punctuation,
}

impl SyntaxRole {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::Function => "function",
            Self::Type => "type",
            Self::String => "string",
            Self::Constant => "constant",
            Self::Comment => "comment",
            Self::Property => "property",
            Self::Punctuation => "punctuation",
        }
    }
}

impl fmt::Display for SyntaxRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThemeSyntax {
    pub keyword: Option<ThemeColor>,
    pub function: Option<ThemeColor>,
    pub r#type: Option<ThemeColor>,
    pub string: Option<ThemeColor>,
    pub constant: Option<ThemeColor>,
    pub comment: Option<ThemeColor>,
    pub property: Option<ThemeColor>,
    pub punctuation: Option<ThemeColor>,
}

impl ThemeSyntax {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.keyword.is_none()
            && self.function.is_none()
            && self.r#type.is_none()
            && self.string.is_none()
            && self.constant.is_none()
            && self.comment.is_none()
            && self.property.is_none()
            && self.punctuation.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeDocument {
    name: String,
    colors: ThemeColors,
    syntax: ThemeSyntax,
    source: SourceIdentity,
}

impl ThemeDocument {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn colors(&self) -> &ThemeColors {
        &self.colors
    }

    #[must_use]
    pub const fn syntax(&self) -> &ThemeSyntax {
        &self.syntax
    }

    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }
}

#[must_use]
pub fn compiled_theme() -> ThemeDocument {
    const fn rgb(r: u8, g: u8, b: u8) -> ThemeColor {
        ThemeColor::Rgb(Rgb { r, g, b })
    }
    ThemeDocument {
        name: TERMINAL_THEME.to_owned(),
        colors: ThemeColors {
            text: ThemeColor::Ansi(AnsiColor::White),
            muted: ThemeColor::Ansi(AnsiColor::DarkGrey),
            accent: ThemeColor::Ansi(AnsiColor::Cyan),
            brand: rgb(0xff, 0x9f, 0x43),
            warning: ThemeColor::Ansi(AnsiColor::Yellow),
            error: ThemeColor::Ansi(AnsiColor::Red),
            success: ThemeColor::Ansi(AnsiColor::Green),
            surface: rgb(0x26, 0x28, 0x30),
        },
        syntax: ThemeSyntax::default(),
        source: SourceIdentity::virtual_source(SourceKind::Compiled, "compiled theme terminal"),
    }
}

fn compiled_document(name: &str, content: &str) -> Result<ThemeDocument, ConfigError> {
    let source =
        SourceIdentity::virtual_source(SourceKind::Compiled, format!("compiled theme {name}"));
    let ParsedTheme { colors, syntax } = Document::parse(content, &source)?;
    Ok(ThemeDocument {
        name: name.to_owned(),
        colors,
        syntax,
        source,
    })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RolesDocument {
    text: String,
    muted: String,
    accent: String,
    brand: String,
    warning: String,
    error: String,
    success: String,
    surface: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyntaxDocument {
    keyword: Option<String>,
    function: Option<String>,
    r#type: Option<String>,
    string: Option<String>,
    constant: Option<String>,
    comment: Option<String>,
    property: Option<String>,
    punctuation: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    #[serde(default)]
    defs: BTreeMap<String, String>,
    colors: RolesDocument,
    #[serde(default)]
    syntax: SyntaxDocument,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ThemeColorFault {
    #[error("refers to `{0}`, which is neither a `defs` alias nor a `#RRGGBB` literal")]
    Unresolved(String),
    #[error("has an alias cycle through `{0}`")]
    AliasCycle(String),
}

impl Document {
    fn parse(content: &str, source: &SourceIdentity) -> Result<ParsedTheme, ConfigError> {
        let options = Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
        let document: Self = options
            .from_str(content)
            .map_err(|error| ConfigError::Parse {
                origin: source.clone(),
                message: error.to_string(),
            })?;
        if document.version != 1 {
            return Err(ConfigError::UnsupportedVersion {
                origin: source.clone(),
                version: document.version,
            });
        }
        let resolve = |value: &str| -> Result<ThemeColor, ThemeColorFault> {
            let mut seen = BTreeSet::new();
            let mut current = value;
            loop {
                if let Some(rgb) = Rgb::parse(current) {
                    return Ok(ThemeColor::Rgb(rgb));
                }
                if !seen.insert(current.to_owned()) {
                    return Err(ThemeColorFault::AliasCycle(current.to_owned()));
                }
                match document.defs.get(current) {
                    Some(next) => current = next,
                    None => return Err(ThemeColorFault::Unresolved(current.to_owned())),
                }
            }
        };
        let role = |role: &str, value: &str| -> Result<ThemeColor, ConfigError> {
            resolve(value).map_err(|fault| ConfigError::Parse {
                origin: source.clone(),
                message: format!("theme role `{role}` {fault}"),
            })
        };
        let roles = &document.colors;
        let colors = ThemeColors {
            text: role("text", &roles.text)?,
            muted: role("muted", &roles.muted)?,
            accent: role("accent", &roles.accent)?,
            brand: role("brand", &roles.brand)?,
            warning: role("warning", &roles.warning)?,
            error: role("error", &roles.error)?,
            success: role("success", &roles.success)?,
            surface: role("surface", &roles.surface)?,
        };
        let syntax_role =
            |role: SyntaxRole, value: &Option<String>| -> Result<Option<ThemeColor>, ConfigError> {
                match value {
                    None => Ok(None),
                    Some(value) => {
                        resolve(value)
                            .map(Some)
                            .map_err(|reason| ConfigError::InvalidThemeSyntax {
                                origin: source.clone(),
                                role,
                                reason,
                            })
                    }
                }
            };
        let block = &document.syntax;
        let syntax = ThemeSyntax {
            keyword: syntax_role(SyntaxRole::Keyword, &block.keyword)?,
            function: syntax_role(SyntaxRole::Function, &block.function)?,
            r#type: syntax_role(SyntaxRole::Type, &block.r#type)?,
            string: syntax_role(SyntaxRole::String, &block.string)?,
            constant: syntax_role(SyntaxRole::Constant, &block.constant)?,
            comment: syntax_role(SyntaxRole::Comment, &block.comment)?,
            property: syntax_role(SyntaxRole::Property, &block.property)?,
            punctuation: syntax_role(SyntaxRole::Punctuation, &block.punctuation)?,
        };
        Ok(ParsedTheme { colors, syntax })
    }
}

struct ParsedTheme {
    colors: ThemeColors,
    syntax: ThemeSyntax,
}

fn theme_paths(
    loader: &ConfigLoader,
    cwd: &Path,
    probes: &mut Probes,
) -> Vec<(std::path::PathBuf, SourceKind)> {
    let mut directories = vec![(loader.paths.global_dir.join("themes"), SourceKind::Global)];
    for directory in project_directories(cwd, probes) {
        directories.push((directory.join(".qq/themes"), SourceKind::Project));
    }
    directories
}

pub(super) fn load(
    loader: &ConfigLoader,
    cwd: &Path,
    name: &str,
    truecolor: TruecolorSupport,
) -> Result<ThemeDocument, ConfigError> {
    let name = if name == DEFAULT_THEME {
        default_theme_name(truecolor)
    } else {
        name
    };
    if name == TERMINAL_THEME {
        return Ok(compiled_theme());
    }
    validate_name(name)?;
    let cwd = canonical_working_directory(cwd)?;
    let mut probes = Probes::default();
    let mut found = None;
    for (directory, kind) in theme_paths(loader, &cwd, &mut probes) {
        if let Some(candidate) = discover_file(
            directory.join(format!("{name}.ron")),
            kind,
            false,
            &mut probes,
        )? {
            found = Some(candidate);
        }
    }
    let Some(candidate) = found else {
        return match COMPILED_THEMES
            .iter()
            .find(|(compiled, _)| *compiled == name)
        {
            Some((compiled, content)) => compiled_document(compiled, content),
            None => Err(ConfigError::UnknownTheme {
                name: name.to_owned(),
            }),
        };
    };
    let (source, content) = read_candidate(&candidate)?;
    let ParsedTheme { colors, syntax } = Document::parse(&content, &source)?;
    Ok(ThemeDocument {
        name: name.to_owned(),
        colors,
        syntax,
        source,
    })
}

pub(super) fn discover(
    loader: &ConfigLoader,
    cwd: &Path,
) -> Result<Vec<ThemeDocument>, ConfigError> {
    let cwd = canonical_working_directory(cwd)?;
    let mut probes = Probes::default();
    let mut themes: BTreeMap<String, ThemeDocument> = BTreeMap::new();
    themes.insert(TERMINAL_THEME.to_owned(), compiled_theme());
    for (name, content) in COMPILED_THEMES {
        let document = compiled_document(name, content)?;
        themes.insert((*name).to_owned(), document);
    }
    for (directory, kind) in theme_paths(loader, &cwd, &mut probes) {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("ron") {
                    return None;
                }
                let stem = path.file_stem()?.to_str()?.to_owned();
                validate_name(&stem).ok().map(|()| stem)
            })
            .collect();
        names.sort();
        for name in names.into_iter().take(MAX_DISCOVERED_THEMES) {
            if name == DEFAULT_THEME || name == TERMINAL_THEME {
                continue;
            }
            let Some(candidate) = discover_file(
                directory.join(format!("{name}.ron")),
                kind,
                false,
                &mut probes,
            )?
            else {
                continue;
            };
            let Ok((source, content)) = read_candidate(&candidate) else {
                continue;
            };
            if let Ok(ParsedTheme { colors, syntax }) = Document::parse(&content, &source) {
                themes.insert(
                    name.clone(),
                    ThemeDocument {
                        name,
                        colors,
                        syntax,
                        source,
                    },
                );
            }
        }
    }
    Ok(themes.into_values().collect())
}

fn validate_name(name: &str) -> Result<(), ConfigError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(ConfigError::UnknownTheme {
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}
