use std::{fmt, str::FromStr};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    ToggleNavigator,
    CreateRootSession,
    CreateChildSession,
    CancelRun,
    InterruptRun,
}

impl Action {
    const ALL: [Self; 5] = [
        Self::ToggleNavigator,
        Self::CreateRootSession,
        Self::CreateChildSession,
        Self::CancelRun,
        Self::InterruptRun,
    ];

    /// Every bindable action.
    pub fn all() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyChord {
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// The key event this chord describes, for tests that drive the app.
    #[cfg(test)]
    pub(crate) fn to_event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }

    pub(crate) fn matches(self, key: KeyEvent) -> bool {
        let code = match key.code {
            KeyCode::Char(character) => KeyCode::Char(character.to_ascii_lowercase()),
            code => code,
        };
        code == self.code && key.modifiers == self.modifiers
    }
}

impl FromStr for KeyChord {
    type Err = SettingsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut modifiers = KeyModifiers::NONE;
        let mut code = None;
        // A trailing `-` names the minus key itself (`Alt--`), which a plain
        // split would read as an empty token.
        let (value_parts, literal_minus) = match value.strip_suffix("--") {
            Some(prefix) => (format!("{prefix}-"), true),
            None => (value.to_owned(), false),
        };
        let mut parts: Vec<&str> = value_parts.split('-').collect();
        if literal_minus {
            parts.pop();
            parts.push("-");
        }
        for part in parts {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" if !modifiers.contains(KeyModifiers::CONTROL) => {
                    modifiers.insert(KeyModifiers::CONTROL);
                }
                "alt" if !modifiers.contains(KeyModifiers::ALT) => {
                    modifiers.insert(KeyModifiers::ALT);
                }
                "shift" if !modifiers.contains(KeyModifiers::SHIFT) => {
                    modifiers.insert(KeyModifiers::SHIFT);
                }
                token if code.is_none() => code = Some(parse_key_code(token)?),
                _ => return Err(SettingsError::InvalidKeyChord(value.to_owned())),
            }
        }
        let code = code.ok_or_else(|| SettingsError::InvalidKeyChord(value.to_owned()))?;
        if matches!(code, KeyCode::Char(_)) && modifiers.is_empty() {
            return Err(SettingsError::UnmodifiedCharacter(value.to_owned()));
        }
        Ok(Self { code, modifiers })
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            formatter.write_str("Ctrl-")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            formatter.write_str("Alt-")?;
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            formatter.write_str("Shift-")?;
        }
        match self.code {
            KeyCode::Char(character) => write!(formatter, "{}", character.to_ascii_uppercase()),
            KeyCode::F(number) => write!(formatter, "F{number}"),
            KeyCode::Tab => formatter.write_str("Tab"),
            KeyCode::BackTab => formatter.write_str("BackTab"),
            KeyCode::Enter => formatter.write_str("Enter"),
            KeyCode::Esc => formatter.write_str("Esc"),
            KeyCode::Up => formatter.write_str("Up"),
            KeyCode::Down => formatter.write_str("Down"),
            KeyCode::Left => formatter.write_str("Left"),
            KeyCode::Right => formatter.write_str("Right"),
            KeyCode::PageUp => formatter.write_str("PageUp"),
            KeyCode::PageDown => formatter.write_str("PageDown"),
            KeyCode::Home => formatter.write_str("Home"),
            KeyCode::End => formatter.write_str("End"),
            KeyCode::Delete => formatter.write_str("Delete"),
            KeyCode::Backspace => formatter.write_str("Backspace"),
            _ => formatter.write_str("Key"),
        }
    }
}

fn parse_key_code(value: &str) -> Result<KeyCode, SettingsError> {
    match value {
        "tab" => Ok(KeyCode::Tab),
        "backtab" => Ok(KeyCode::BackTab),
        "enter" => Ok(KeyCode::Enter),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "pageup" => Ok(KeyCode::PageUp),
        "pagedown" => Ok(KeyCode::PageDown),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "delete" | "del" => Ok(KeyCode::Delete),
        "backspace" => Ok(KeyCode::Backspace),
        value if value.len() == 1 => Ok(KeyCode::Char(
            value.chars().next().expect("one-byte key has a character"),
        )),
        value if value.starts_with('f') => value[1..]
            .parse::<u8>()
            .ok()
            .filter(|number| (1..=24).contains(number))
            .map(KeyCode::F)
            .ok_or_else(|| SettingsError::InvalidKeyChord(value.to_owned())),
        _ => Err(SettingsError::InvalidKeyChord(value.to_owned())),
    }
}

/// One item of the top row's right-hand status, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusItem {
    Model,
    /// The agent profile in effect, shown only when it is not `default`.
    Profile,
    /// The approval mode in effect, shown only when it is not `auto`.
    ApprovalMode,
    /// The reasoning effort pin in effect, shown only when one is set.
    Effort,
    Context,
    Cost,
    Workspace,
    Tools,
}

impl StatusItem {
    /// The default status line: model, profile, approval mode, context
    /// occupancy, cost.
    pub const DEFAULT: [Self; 6] = [
        Self::Model,
        Self::Profile,
        Self::ApprovalMode,
        Self::Effort,
        Self::Context,
        Self::Cost,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    bindings: Vec<(Action, Vec<KeyChord>)>,
    status_line: Vec<StatusItem>,
}

impl Settings {
    #[must_use]
    pub fn action_for(&self, key: KeyEvent) -> Option<Action> {
        self.bindings.iter().find_map(|(action, chords)| {
            chords
                .iter()
                .any(|chord| chord.matches(key))
                .then_some(*action)
        })
    }

    #[must_use]
    pub fn binding_label(&self, action: Action) -> Option<String> {
        self.bindings
            .iter()
            .find(|(candidate, _)| *candidate == action)
            .and_then(|(_, bindings)| bindings.first())
            .map(ToString::to_string)
    }

    #[must_use]
    pub fn bindings(&self) -> &[(Action, Vec<KeyChord>)] {
        &self.bindings
    }

    /// Items shown at the right of the top row, in order.
    #[must_use]
    pub fn status_line(&self) -> &[StatusItem] {
        &self.status_line
    }
}

impl Default for Settings {
    fn default() -> Self {
        SettingsBuilder::default()
            .build()
            .expect("default TUI bindings are valid")
    }
}

#[derive(Debug, Clone)]
pub struct SettingsBuilder {
    bindings: Vec<(Action, Vec<KeyChord>)>,
    status_line: Vec<StatusItem>,
}

impl Default for SettingsBuilder {
    fn default() -> Self {
        let binding = |action, values: &[&str]| {
            (
                action,
                values
                    .iter()
                    .map(|value| value.parse().expect("default key chord is valid"))
                    .collect(),
            )
        };
        Self {
            status_line: StatusItem::DEFAULT.to_vec(),
            bindings: vec![
                binding(Action::ToggleNavigator, &["Ctrl-T"]),
                binding(Action::CreateRootSession, &["Alt-N"]),
                binding(Action::CreateChildSession, &["Alt-C"]),
                binding(Action::CancelRun, &["Ctrl-X"]),
                binding(Action::InterruptRun, &["Alt-S"]),
            ],
        }
    }
}

impl SettingsBuilder {
    /// Replace the top-row status items.
    #[must_use]
    pub fn status_line(mut self, items: Vec<StatusItem>) -> Self {
        self.status_line = items;
        self
    }

    #[must_use]
    pub fn bindings(mut self, action: Action, bindings: Vec<KeyChord>) -> Self {
        if let Some((_, current)) = self
            .bindings
            .iter_mut()
            .find(|(candidate, _)| *candidate == action)
        {
            *current = bindings;
        } else {
            self.bindings.push((action, bindings));
        }
        self
    }

    pub fn build(mut self) -> Result<Settings, SettingsError> {
        self.bindings.sort_by_key(|(action, _)| *action);
        for action in Action::ALL {
            if !self
                .bindings
                .iter()
                .any(|(candidate, _)| *candidate == action)
            {
                self.bindings.push((action, Vec::new()));
            }
        }
        for (index, (action, chords)) in self.bindings.iter().enumerate() {
            for chord in chords {
                if *chord == KeyChord::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
                    || *chord == KeyChord::new(KeyCode::Enter, KeyModifiers::NONE)
                {
                    return Err(SettingsError::ReservedKeyChord(chord.to_string()));
                }
                if let Some((other, _)) =
                    self.bindings
                        .iter()
                        .skip(index + 1)
                        .find(|(_, other_chords)| {
                            other_chords.iter().any(|candidate| candidate == chord)
                        })
                {
                    return Err(SettingsError::BindingCollision {
                        chord: chord.to_string(),
                        first: *action,
                        second: *other,
                    });
                }
            }
        }
        Ok(Settings {
            bindings: self.bindings,
            status_line: self.status_line,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SettingsError {
    #[error("invalid key chord {0:?}")]
    InvalidKeyChord(String),
    #[error("unmodified character binding {0:?} would steal composer input")]
    UnmodifiedCharacter(String),
    #[error("key chord {0} is reserved by the TUI")]
    ReservedKeyChord(String),
    #[error("key chord {chord} is assigned to both {first:?} and {second:?}")]
    BindingCollision {
        chord: String,
        first: Action,
        second: Action,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_modified_and_function_keys() {
        assert_eq!("Ctrl-N".parse::<KeyChord>().unwrap().to_string(), "Ctrl-N");
        assert_eq!("F12".parse::<KeyChord>().unwrap().to_string(), "F12");
        assert!(matches!(
            "x".parse::<KeyChord>(),
            Err(SettingsError::UnmodifiedCharacter(_))
        ));
    }

    #[test]
    fn rejects_colliding_bindings() {
        let error = SettingsBuilder::default()
            .bindings(Action::CreateChildSession, vec!["Ctrl-T".parse().unwrap()])
            .build()
            .unwrap_err();

        assert!(matches!(error, SettingsError::BindingCollision { .. }));
    }

    #[test]
    fn rejects_the_always_quit_binding() {
        let error = SettingsBuilder::default()
            .bindings(Action::CancelRun, vec!["Ctrl-C".parse().unwrap()])
            .build()
            .unwrap_err();

        assert_eq!(error, SettingsError::ReservedKeyChord("Ctrl-C".to_owned()));
    }

    #[test]
    fn rejects_the_prompt_submission_binding() {
        let error = SettingsBuilder::default()
            .bindings(Action::CancelRun, vec!["Enter".parse().unwrap()])
            .build()
            .unwrap_err();

        assert_eq!(error, SettingsError::ReservedKeyChord("Enter".to_owned()));
    }
}
