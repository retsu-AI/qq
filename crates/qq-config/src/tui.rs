use std::{collections::BTreeMap, fmt, path::Path};

use ron::{Options, extensions::Extensions};
use serde::Deserialize;

use super::{
    ConfigError, ConfigLoader, SourceIdentity, SourceKind,
    loader::{
        Probes, canonical_working_directory, discover_file, project_directories, read_candidate,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TuiAction {
    ToggleNavigator,
    CreateRootSession,
    CreateChildSession,
    CancelRun,
    InterruptRun,
}

impl TuiAction {
    const ALL: [Self; 5] = [
        Self::ToggleNavigator,
        Self::CreateRootSession,
        Self::CreateChildSession,
        Self::CancelRun,
        Self::InterruptRun,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfigSettings {
    theme: String,
    bindings: Vec<(TuiAction, Vec<String>)>,
}

impl TuiConfigSettings {
    #[must_use]
    pub fn bindings(&self) -> &[(TuiAction, Vec<String>)] {
        &self.bindings
    }

    /// The selected theme name; the `qq` default alias when no layer set one
    /// (see `theme::default_theme_name` for what it resolves to).
    #[must_use]
    pub fn theme(&self) -> &str {
        &self.theme
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfigDefaults(TuiConfigSettings);

impl TuiConfigDefaults {
    pub fn new(
        bindings: impl IntoIterator<Item = (TuiAction, Vec<String>)>,
    ) -> Result<Self, ConfigError> {
        let bindings = bindings.into_iter().collect::<BTreeMap<_, _>>();
        if TuiAction::ALL
            .iter()
            .any(|action| !bindings.contains_key(action))
        {
            return Err(ConfigError::InvalidTuiSettings {
                message: "compiled TUI defaults must contain every action".to_owned(),
            });
        }
        Ok(Self(TuiConfigSettings {
            theme: super::theme::DEFAULT_THEME.to_owned(),
            bindings: bindings.into_iter().collect(),
        }))
    }

    #[must_use]
    pub const fn settings(&self) -> &TuiConfigSettings {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiConfigKey {
    Theme,
    Binding(TuiAction),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiSourceReport {
    source: SourceIdentity,
    touched: Vec<TuiConfigKey>,
}

impl TuiSourceReport {
    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }

    #[must_use]
    pub fn touched(&self) -> &[TuiConfigKey] {
        &self.touched
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfigProvenance {
    theme: SourceIdentity,
    bindings: BTreeMap<TuiAction, SourceIdentity>,
}

impl TuiConfigProvenance {
    #[must_use]
    pub const fn theme(&self) -> &SourceIdentity {
        &self.theme
    }

    #[must_use]
    pub fn binding(&self, action: TuiAction) -> &SourceIdentity {
        self.bindings
            .get(&action)
            .expect("every TUI action has a compiled default")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfigSnapshot {
    settings: TuiConfigSettings,
    reports: Vec<TuiSourceReport>,
    provenance: TuiConfigProvenance,
}

impl TuiConfigSnapshot {
    #[must_use]
    pub const fn settings(&self) -> &TuiConfigSettings {
        &self.settings
    }

    #[must_use]
    pub fn source_reports(&self) -> &[TuiSourceReport] {
        &self.reports
    }

    #[must_use]
    pub const fn provenance(&self) -> &TuiConfigProvenance {
        &self.provenance
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BindingsDocument {
    toggle_navigator: Option<Vec<String>>,
    create_root_session: Option<Vec<String>>,
    create_child_session: Option<Vec<String>>,
    cancel_run: Option<Vec<String>>,
    interrupt_run: Option<Vec<String>>,
}

impl BindingsDocument {
    fn entries(&self) -> [(TuiAction, Option<&[String]>); 5] {
        [
            (TuiAction::ToggleNavigator, self.toggle_navigator.as_deref()),
            (
                TuiAction::CreateRootSession,
                self.create_root_session.as_deref(),
            ),
            (
                TuiAction::CreateChildSession,
                self.create_child_session.as_deref(),
            ),
            (TuiAction::CancelRun, self.cancel_run.as_deref()),
            (TuiAction::InterruptRun, self.interrupt_run.as_deref()),
        ]
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Document {
    version: u32,
    theme: Option<String>,
    bindings: BindingsDocument,
}

impl Document {
    fn parse<Validate, ValidationError>(
        content: &str,
        source: &SourceIdentity,
        validate_binding: &Validate,
    ) -> Result<Self, ConfigError>
    where
        Validate: Fn(&str) -> Result<(), ValidationError>,
        ValidationError: fmt::Display,
    {
        let options = Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
        let document: Self = options
            .from_str(content)
            .map_err(|error| ConfigError::Parse {
                origin: source.clone(),
                message: explain_removed_key(&error.to_string()),
            })?;
        if document.version != 1 {
            return Err(ConfigError::UnsupportedVersion {
                origin: source.clone(),
                version: document.version,
            });
        }
        for (_, values) in document.bindings.entries() {
            let Some(values) = values else {
                continue;
            };
            for value in values {
                validate_binding(value).map_err(|error| ConfigError::Parse {
                    origin: source.clone(),
                    message: error.to_string(),
                })?;
            }
        }
        Ok(document)
    }

    fn touched(&self) -> Vec<TuiConfigKey> {
        let mut touched = Vec::new();
        if self.theme.is_some() {
            touched.push(TuiConfigKey::Theme);
        }
        touched.extend(
            self.bindings
                .entries()
                .into_iter()
                .filter_map(|(action, values)| values.map(|_| TuiConfigKey::Binding(action))),
        );
        touched
    }
}

/// Keys the TUI used to accept and now rejects. The parser's "unexpected
/// field" error names the key; this adds what to do about it, so a config
/// written for an older `qq` fails with instructions rather than a schema.
const REMOVED_KEYS: [(&str, &str); 5] = [
    (
        "layout",
        "the TUI has one transcript layout now; delete the `layout` key",
    ),
    (
        "select_threadline",
        "layouts were removed; delete the `select_threadline` binding",
    ),
    (
        "select_fold_focus",
        "layouts were removed; delete the `select_fold_focus` binding",
    ),
    (
        "next_layout",
        "layouts were removed; delete the `next_layout` binding",
    ),
    (
        "previous_layout",
        "layouts were removed; delete the `previous_layout` binding",
    ),
];

fn explain_removed_key(message: &str) -> String {
    for (key, advice) in REMOVED_KEYS {
        if message.contains(&format!("`{key}`")) {
            return format!("{message}; {advice}");
        }
    }
    message.to_owned()
}

pub(super) fn load<Validate, ValidationError>(
    loader: &ConfigLoader,
    cwd: &Path,
    defaults: &TuiConfigDefaults,
    validate_binding: &Validate,
) -> Result<TuiConfigSnapshot, ConfigError>
where
    Validate: Fn(&str) -> Result<(), ValidationError>,
    ValidationError: fmt::Display,
{
    let cwd = canonical_working_directory(cwd)?;
    let compiled = SourceIdentity::virtual_source(SourceKind::Compiled, "compiled TUI defaults");
    let mut theme = defaults.settings().theme().to_owned();
    let mut bindings: BTreeMap<_, _> = defaults.settings().bindings().iter().cloned().collect();
    let mut provenance = TuiConfigProvenance {
        theme: compiled.clone(),
        bindings: bindings
            .keys()
            .map(|action| (*action, compiled.clone()))
            .collect(),
    };
    let mut reports = vec![TuiSourceReport {
        source: compiled,
        touched: [TuiConfigKey::Theme]
            .into_iter()
            .chain(bindings.keys().copied().map(TuiConfigKey::Binding))
            .collect(),
    }];

    let mut candidates = Vec::new();
    let mut probes = Probes::default();
    if let Some(global) = discover_file(
        loader.paths.global_dir.join("tui.ron"),
        SourceKind::Global,
        false,
        &mut probes,
    )? {
        candidates.push(global);
    }
    for directory in project_directories(&cwd, &mut probes) {
        if let Some(project) = discover_file(
            directory.join(".qq/tui.ron"),
            SourceKind::Project,
            false,
            &mut probes,
        )? {
            candidates.push(project);
        }
    }

    for candidate in candidates {
        let (source, content) = read_candidate(&candidate)?;
        let document = Document::parse(&content, &source, validate_binding)?;
        if let Some(incoming) = &document.theme {
            theme.clone_from(incoming);
            provenance.theme = source.clone();
        }
        for (action, values) in document.bindings.entries() {
            let Some(values) = values else {
                continue;
            };
            bindings.insert(action, values.to_vec());
            provenance.bindings.insert(action, source.clone());
        }
        reports.push(TuiSourceReport {
            source,
            touched: document.touched(),
        });
    }

    let settings = TuiConfigSettings {
        theme,
        bindings: bindings.into_iter().collect(),
    };
    Ok(TuiConfigSnapshot {
        settings,
        reports,
        provenance,
    })
}
