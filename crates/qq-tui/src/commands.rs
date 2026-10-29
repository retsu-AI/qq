use std::borrow::Cow;

use crossterm::event::KeyEvent;

use crate::settings::{Action, KeyChord, Settings};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Command {
    OpenHelp,
    OpenCommands,
    OpenModels,
    OpenProfiles,
    OpenApprovalModes,
    OpenEffort,
    OpenSkills,
    OpenThemes,
    OpenSessions,
    OpenAgents,
    ToggleSessions,
    NewRootSession,
    NewChildSession,
    CompactSession,
    RollbackCompaction,
    CancelRun,
    ToggleToolDetail,
    CursorUp,
    CursorDown,
    ToggleReasoning,
    ToggleSidebar,
    ToggleInspector,
    ToggleMouse,
    ShowAttention,
    ShowChanges,
    ShowSideQuestions,
    ShowGoal,
    NewSideThread,
    CancelSideQuestion,
    FocusParent,
    FocusFirstChild,
    FocusNextSibling,
    FocusPreviousSibling,
    FocusNextApproval,
    ApproveBackground,
    DenyBackground,
    QueueDraft,
    DequeueDraft,
    SteerRun,
    InterruptRun,
    OpenEditor,
    SearchHistory,
    PruneSessions,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Category {
    Help,
    Session,
    Run,
    Model,
    View,
    Compose,
    System,
}

impl Category {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Help => "HELP",
            Self::Session => "SESSIONS",
            Self::Run => "RUN",
            Self::Model => "MODEL",
            Self::View => "VIEW",
            Self::Compose => "COMPOSER",
            Self::System => "SYSTEM",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandSpec {
    pub command: Command,
    pub title: &'static str,
    pub category: Category,
    pub slash: &'static [&'static str],
    pub chords: &'static [&'static str],
    pub action: Option<Action>,
}

macro_rules! spec {
    ($command:ident, $title:literal, $category:ident, [$($slash:literal),*], [$($chord:literal),*] $(, $action:ident)?) => {
        CommandSpec {
            command: Command::$command,
            title: $title,
            category: Category::$category,
            slash: &[$($slash),*],
            chords: &[$($chord),*],
            action: spec!(@action $($action)?),
        }
    };
    (@action) => { None };
    (@action $action:ident) => { Some(Action::$action) };
}

pub(crate) const COMMANDS: [CommandSpec; 44] = [
    spec!(
        OpenHelp,
        "show every command and key",
        Help,
        ["/help"],
        ["F1"]
    ),
    spec!(
        OpenCommands,
        "open the command palette",
        Help,
        ["/commands"],
        ["Ctrl-K"]
    ),
    spec!(
        OpenSessions,
        "open sessions",
        Session,
        ["/sessions", "/resume"],
        []
    ),
    spec!(
        OpenAgents,
        "open the focused session's agent tree",
        Session,
        ["/agents"],
        []
    ),
    spec!(
        ToggleSessions,
        "toggle the session navigator",
        Session,
        [],
        [],
        ToggleNavigator
    ),
    spec!(
        NewRootSession,
        "create a session",
        Session,
        ["/new", "/clear"],
        [],
        CreateRootSession
    ),
    spec!(
        NewChildSession,
        "create a child session",
        Session,
        [],
        [],
        CreateChildSession
    ),
    spec!(
        CompactSession,
        "compact session context",
        Session,
        ["/compact"],
        []
    ),
    spec!(
        RollbackCompaction,
        "undo the newest compaction",
        Session,
        ["/rollback"],
        []
    ),
    spec!(
        PruneSessions,
        "delete every empty session",
        Session,
        ["/prune"],
        []
    ),
    spec!(FocusParent, "focus the parent session", Session, [], []),
    spec!(
        FocusFirstChild,
        "focus the first child session",
        Session,
        [],
        ["Alt-Down"]
    ),
    spec!(
        FocusNextSibling,
        "focus the next sibling session",
        Session,
        [],
        ["Alt-Right"]
    ),
    spec!(
        FocusPreviousSibling,
        "focus the previous sibling session",
        Session,
        [],
        ["Alt-Left"]
    ),
    spec!(
        FocusNextApproval,
        "jump to the next session that needs you",
        Session,
        [],
        ["Ctrl-G"]
    ),
    spec!(
        ApproveBackground,
        "approve the waiting call in another session",
        Session,
        [],
        ["Alt-A"]
    ),
    spec!(
        DenyBackground,
        "deny the waiting call in another session",
        Session,
        [],
        ["Alt-D"]
    ),
    spec!(CancelRun, "cancel the active run", Run, [], [], CancelRun),
    spec!(SteerRun, "steer the active run with the draft", Run, [], []),
    spec!(
        InterruptRun,
        "interrupt the active run and steer it with the draft",
        Run,
        [],
        [],
        InterruptRun
    ),
    spec!(
        QueueDraft,
        "queue the draft until the run finishes",
        Run,
        [],
        ["Ctrl-Enter", "Ctrl-Q"]
    ),
    spec!(
        DequeueDraft,
        "edit the newest queued draft",
        Run,
        [],
        ["Alt-Up"]
    ),
    spec!(OpenModels, "choose a model", Model, ["/models"], []),
    spec!(
        OpenProfiles,
        "choose an agent profile",
        Model,
        ["/profile"],
        []
    ),
    spec!(
        OpenApprovalModes,
        "choose an approval mode",
        Model,
        ["/approval"],
        []
    ),
    spec!(
        OpenEffort,
        "choose reasoning effort",
        Model,
        ["/effort"],
        []
    ),
    spec!(
        OpenSkills,
        "list workspace commands and skills",
        Model,
        ["/skills"],
        []
    ),
    spec!(OpenThemes, "choose a theme", View, ["/theme"], []),
    spec!(
        ToggleToolDetail,
        "toggle tool call detail",
        View,
        [],
        ["Ctrl-O"]
    ),
    spec!(
        CursorUp,
        "select the previous tool call",
        View,
        [],
        ["Ctrl-Up"]
    ),
    spec!(
        CursorDown,
        "select the next tool call",
        View,
        [],
        ["Ctrl-Down"]
    ),
    spec!(
        ToggleReasoning,
        "toggle reasoning detail",
        View,
        [],
        ["Alt-R"]
    ),
    spec!(
        ToggleSidebar,
        "toggle the session sidebar",
        View,
        [],
        ["Ctrl-\\"]
    ),
    spec!(
        ToggleInspector,
        "toggle the inspector pane",
        View,
        [],
        ["Alt-I"]
    ),
    spec!(ToggleMouse, "toggle mouse capture", View, ["/mouse"], []),
    spec!(
        ShowAttention,
        "show everything that needs you",
        View,
        ["/attention"],
        []
    ),
    spec!(
        ShowChanges,
        "show every file agents changed",
        View,
        ["/changes"],
        []
    ),
    spec!(
        OpenEditor,
        "edit the draft in $EDITOR",
        Compose,
        ["/editor"],
        ["Alt-E"]
    ),
    spec!(
        SearchHistory,
        "search prompt history",
        Compose,
        [],
        ["Ctrl-R"]
    ),
    spec!(Quit, "exit QQ", System, ["/quit", "/exit"], ["Ctrl-C"]),
    spec!(
        ShowGoal,
        "view session goal and progress",
        View,
        ["/goal"],
        []
    ),
    spec!(
        ShowSideQuestions,
        "ask or view side questions",
        View,
        ["/btw", "/ask"],
        []
    ),
    spec!(
        NewSideThread,
        "ask in a new side thread",
        View,
        ["/btw-new"],
        []
    ),
    spec!(
        CancelSideQuestion,
        "cancel only the side question",
        Run,
        ["/btw-cancel"],
        []
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlashAction {
    Client(Command),
    WorkspaceCommand,
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlashEntry {
    pub name: Cow<'static, str>,
    pub title: Cow<'static, str>,
    pub action: SlashAction,
}

pub(crate) fn slash_entries() -> impl Iterator<Item = SlashEntry> {
    COMMANDS.iter().flat_map(|spec| {
        spec.slash.iter().map(move |name| SlashEntry {
            name: Cow::Borrowed(name),
            title: Cow::Borrowed(spec.title),
            action: SlashAction::Client(spec.command),
        })
    })
}

pub fn slash_names() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().flat_map(|spec| spec.slash.iter().copied())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandRow {
    pub title: &'static str,
    pub slash: &'static [&'static str],
    pub default_chords: &'static [&'static str],
}

pub fn command_rows() -> impl Iterator<Item = CommandRow> {
    COMMANDS.iter().map(|spec| CommandRow {
        title: spec.title,
        slash: spec.slash,
        default_chords: match spec.action {
            Some(action) => crate::settings::default_chords(action),
            None => spec.chords,
        },
    })
}

pub(crate) fn matching_slash_entries(
    token: &str,
    extra: impl Iterator<Item = SlashEntry>,
) -> Vec<SlashEntry> {
    if !token.starts_with('/') || token.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let query = &token[1..];
    let mut prefix = Vec::new();
    let mut fuzzy = Vec::new();
    for entry in slash_entries().chain(extra) {
        let name = &entry.name[1..];
        if name.starts_with(query) {
            prefix.push(entry);
        } else if crate::picker::fuzzy_matches(query, name) {
            fuzzy.push(entry);
        }
    }
    prefix.extend(fuzzy);
    prefix
}

pub(crate) fn spec(command: Command) -> &'static CommandSpec {
    COMMANDS
        .iter()
        .find(|spec| spec.command == command)
        .expect("every command has a registry row")
}

pub(crate) fn command_for_action(action: Action) -> Command {
    COMMANDS
        .iter()
        .find(|spec| spec.action == Some(action))
        .map(|spec| spec.command)
        .expect("every keybinding action has a command row")
}

pub(crate) fn command_for_key(settings: &Settings, key: KeyEvent) -> Option<Command> {
    if let Some(action) = settings.action_for(key) {
        return Some(command_for_action(action));
    }
    default_chords()
        .iter()
        .find(|(_, chord)| chord.matches(key))
        .map(|(command, _)| *command)
}

fn default_chords() -> &'static [(Command, KeyChord)] {
    static PARSED: std::sync::LazyLock<Vec<(Command, KeyChord)>> = std::sync::LazyLock::new(|| {
        COMMANDS
            .iter()
            .filter(|spec| spec.action.is_none())
            .flat_map(|spec| {
                spec.chords
                    .iter()
                    .map(|chord| (spec.command, default_chord(chord)))
            })
            .collect()
    });
    &PARSED
}

pub(crate) fn chord_label(settings: &Settings, command: Command) -> Option<String> {
    let spec = spec(command);
    match spec.action {
        Some(action) => settings.binding_label(action),
        None => spec
            .chords
            .first()
            .map(|chord| default_chord(chord).to_string()),
    }
}

fn default_chord(chord: &str) -> KeyChord {
    chord
        .parse()
        .unwrap_or_else(|error| panic!("default chord {chord:?} is valid: {error}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn slash_names_match_the_protocol_reservation_exactly() {
        let here: BTreeSet<String> = slash_entries()
            .map(|entry| entry.name.into_owned())
            .collect();
        let reserved: BTreeSet<String> = qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(here, reserved);
    }

    #[test]
    fn every_action_has_exactly_one_command() {
        for action in Action::all() {
            let rows = COMMANDS
                .iter()
                .filter(|spec| spec.action == Some(action))
                .count();
            assert_eq!(rows, 1, "{action:?}");
        }
    }

    #[test]
    fn every_command_has_one_row_reachable_from_the_palette_and_a_way_in() {
        let contextual = [Command::SteerRun, Command::FocusParent];
        let mut seen = BTreeSet::new();
        for spec in &COMMANDS {
            assert!(seen.insert(spec.command), "{:?} listed twice", spec.command);
            assert!(!spec.title.is_empty());
            let bound = spec.action.is_some()
                || !spec.chords.is_empty()
                || !spec.slash.is_empty()
                || contextual.contains(&spec.command);
            assert!(bound, "{:?} has no chord, action, or slash", spec.command);
        }
    }

    #[test]
    fn default_chords_parse_and_do_not_collide() {
        let settings = Settings::default();
        let mut chords: Vec<(KeyChord, Command)> = Vec::new();
        for spec in &COMMANDS {
            for chord in spec.chords {
                let parsed = default_chord(chord);
                if let Some((_, other)) = chords.iter().find(|(existing, _)| *existing == parsed) {
                    panic!("{chord} bound to both {other:?} and {:?}", spec.command);
                }
                if let Some(action) = settings.action_for(parsed.to_event()) {
                    panic!("{chord} shadows configured action {action:?}");
                }
                chords.push((parsed, spec.command));
            }
        }
    }

    fn matching(token: &str) -> Vec<SlashEntry> {
        matching_slash_entries(token, std::iter::empty())
    }

    fn entry_names(entries: Vec<SlashEntry>) -> Vec<String> {
        entries
            .into_iter()
            .map(|entry| entry.name.into_owned())
            .collect()
    }

    #[test]
    fn slash_matching_requires_a_bare_slash_token() {
        assert!(matching("").is_empty());
        assert!(matching("hello").is_empty());
        assert!(matching("/new ").is_empty());
        assert!(matching("/new arg").is_empty());
        let names = entry_names(matching("/"));
        assert_eq!(
            names.len(),
            qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS.len()
        );
        assert_eq!(names[0], "/help");
        let quit: Vec<_> = matching("/qu")
            .into_iter()
            .map(|entry| entry.action)
            .collect();
        assert_eq!(quit, vec![SlashAction::Client(Command::Quit)]);
    }

    #[test]
    fn slash_matching_prefers_prefixes_then_falls_back_to_subsequences() {
        let names = entry_names(matching("/s"));
        assert_eq!(names[0], "/sessions", "prefix matches lead");
        assert!(
            names.iter().any(|name| name == "/models"),
            "subsequence matches follow"
        );
        assert_eq!(entry_names(matching("/mdl")), vec!["/models"]);
    }

    #[test]
    fn workspace_guidance_joins_the_list_after_client_commands() {
        let extra = [
            SlashEntry {
                name: Cow::Owned("/ship".to_owned()),
                title: Cow::Owned("Ship the branch".to_owned()),
                action: SlashAction::WorkspaceCommand,
            },
            SlashEntry {
                name: Cow::Owned("/sessions-audit".to_owned()),
                title: Cow::Borrowed(""),
                action: SlashAction::Skill,
            },
        ];
        let names = entry_names(matching_slash_entries("/s", extra.iter().cloned()));
        let sessions = names.iter().position(|n| n == "/sessions").unwrap();
        let ship = names.iter().position(|n| n == "/ship").unwrap();
        let audit = names.iter().position(|n| n == "/sessions-audit").unwrap();
        assert!(sessions < ship && ship < audit, "{names:?}");
        assert_eq!(
            entry_names(matching_slash_entries("/shp", extra.iter().cloned())),
            ["/ship"]
        );
    }
}
