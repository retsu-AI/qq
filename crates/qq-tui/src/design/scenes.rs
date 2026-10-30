use super::{
    Block, Composer, DiffLine, Dock, Lane, Mode, Role, Row, Scene, SessionRow, State, Tool, bold,
    span,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesignScene {
    GoalForm,
    GoalReplace,
    Goal,
    GoalCheck,
    MarkdownGallery,
    GoldenPath,
    ToolsExpanded,
    ToolsFolded,
    Approval,
    Reasoning,
    Steering,
    Sessions,
    Fanout,
    Switcher,
    ApprovalDock,
    FirstFrame,
}

impl DesignScene {
    pub const ALL: [Self; 16] = [
        Self::GoalForm,
        Self::GoalReplace,
        Self::Goal,
        Self::GoalCheck,
        Self::MarkdownGallery,
        Self::GoldenPath,
        Self::ToolsExpanded,
        Self::ToolsFolded,
        Self::Approval,
        Self::Reasoning,
        Self::Steering,
        Self::Sessions,
        Self::Fanout,
        Self::Switcher,
        Self::ApprovalDock,
        Self::FirstFrame,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::GoalForm => "goal-form",
            Self::GoalReplace => "goal-replace",
            Self::Goal => "goal",
            Self::GoalCheck => "goal-check",
            Self::MarkdownGallery => "markdown-gallery",
            Self::GoldenPath => "golden-path",
            Self::ToolsExpanded => "tools-expanded",
            Self::ToolsFolded => "tools-folded",
            Self::Approval => "approval",
            Self::Reasoning => "reasoning",
            Self::Steering => "steering",
            Self::Sessions => "sessions",
            Self::Fanout => "fanout",
            Self::Switcher => "switcher",
            Self::ApprovalDock => "approval-dock",
            Self::FirstFrame => "first-frame",
        }
    }

    #[must_use]
    pub fn scene(self) -> Scene {
        match self {
            Self::GoalForm => goal_form(false),
            Self::GoalReplace => goal_form(true),
            Self::Goal => goal(),
            Self::GoalCheck => goal_check(),
            Self::MarkdownGallery => markdown_gallery(),
            Self::GoldenPath => golden_path(),
            Self::ToolsExpanded => tools(true),
            Self::ToolsFolded => tools(false),
            Self::Approval => approval(),
            Self::Reasoning => reasoning(),
            Self::Steering => steering(),
            Self::Sessions => sessions(),
            Self::Fanout => fanout(),
            Self::Switcher => switcher(),
            Self::ApprovalDock => approval_dock(),
            Self::FirstFrame => first_frame(),
        }
    }
}

fn dock(title: &str) -> Dock {
    Dock {
        state: State::Idle,
        activity: String::new(),
        attention: String::new(),
        compact_attention: String::new(),
        panel: Vec::new(),
        queued: Vec::new(),
        composer: Composer::Placeholder,
        mode: Mode::Auto,
        title: title.to_owned(),
        model: "gpt-5.5 high".to_owned(),
        context: "4%".to_owned(),
        cost: String::new(),
        place: "~/qq  main".to_owned(),
        hints: None,
    }
}

fn idle_sessions(focused: &str) -> Vec<SessionRow> {
    vec![session(State::Idle, focused, 0, true, "", "")]
}

fn session(
    state: State,
    title: &str,
    agents: usize,
    focused: bool,
    activity: &str,
    cost: &str,
) -> SessionRow {
    SessionRow {
        state,
        title: title.to_owned(),
        agents,
        focused,
        activity: activity.to_owned(),
        cost: cost.to_owned(),
    }
}

fn tool(state: State, verb: &str, subject: &str, metric: &str) -> Tool {
    Tool {
        state,
        verb: verb.to_owned(),
        subject: subject.to_owned(),
        metric: metric.to_owned(),
        tail: Vec::new(),
        diff: Vec::new(),
    }
}

fn answer(text: &str) -> Block {
    Block::Answer {
        text: text.to_owned(),
        streaming: false,
    }
}

fn keys(pairs: &[(&str, &str)]) -> Composer {
    Composer::Keys(
        pairs
            .iter()
            .map(|(key, label)| ((*key).to_owned(), (*label).to_owned()))
            .collect(),
    )
}

fn heading(text: &str) -> Row {
    vec![bold(text, Role::Text)]
}

fn field(label: &str, value: &str, selected: bool) -> Vec<Row> {
    let marker = if selected {
        span("▎ ", Role::Accent)
    } else {
        span("  ", Role::Text)
    };
    vec![
        vec![marker, span(label, Role::Muted)],
        vec![span("  ", Role::Text), span(value, Role::Text)],
    ]
}

fn goal_form(replace: bool) -> Scene {
    let mut panel = vec![heading(if replace { "Replace goal" } else { "New goal" })];
    if replace {
        panel.push(vec![
            span("replaces  ", Role::Faint),
            span("Finish the billing migration", Role::Muted),
        ]);
    }
    panel.extend(field(
        "Objective",
        "finish the migration and verify the workspace",
        false,
    ));
    panel.extend(field(
        "Check command  optional",
        "cargo test --workspace",
        true,
    ));
    panel.extend(field(
        "Budget",
        "configured defaults · 200 runs · 24h · 256 turns per run",
        false,
    ));
    panel.push(vec![span(
        "A check command runs only after /goal check consent.",
        Role::Faint,
    )]);
    let mut dock = dock("billing");
    dock.panel = panel;
    if replace {
        dock.activity = "goal achieved".to_owned();
        dock.composer = keys(&[("y", "replace"), ("n", "edit"), ("esc", "back")]);
    } else {
        dock.composer = keys(&[
            ("tab", "next field"),
            ("←→", "budget"),
            ("enter", "create"),
            ("esc", "back"),
        ]);
    }
    Scene {
        blocks: Vec::new(),
        sessions: idle_sessions("billing"),
        dock,
    }
}

fn goal() -> Scene {
    let row = |label: &str, value: &str| {
        vec![
            span(format!("{label:<10}"), Role::Faint),
            span(value, Role::Text),
        ]
    };
    let mut dock = dock("billing");
    dock.state = State::NeedsYou;
    dock.activity = "goal needs check consent".to_owned();
    dock.attention = "◇ 1 needs you  ^G".to_owned();
    dock.panel = vec![
        heading("Finish the billing migration"),
        row("runs", "4 / 10"),
        row("tokens", "0 / 1000"),
        row("cost", "$0.00 / $1.00"),
        row("deadline", "2027-01-15 08:00 UTC"),
        row("audits", "1 · 1 failed check"),
        row("check", "cargo test -p billing · 5.0s · consent used"),
        vec![
            span("● ", Role::Faint),
            span("verify invoices", Role::Muted),
        ],
        vec![span("◐ ", Role::Info), span("ship migration", Role::Text)],
        vec![
            span("✕ ", Role::Error),
            span("last check exit 7  missing invoice", Role::Muted),
        ],
        vec![
            span("◇ ", Role::Warning),
            span("completion claim awaiting verification", Role::Text),
        ],
    ];
    dock.composer = keys(&[("c", "check consent"), ("p", "pause"), ("esc", "back")]);
    Scene {
        blocks: vec![
            Block::Prompt("/goal finish the billing migration".to_owned()),
            answer("Backfill verified."),
        ],
        sessions: idle_sessions("billing"),
        dock,
    }
}

fn goal_check() -> Scene {
    let option = |selected: bool, title: &str, detail: &str| {
        let marker = if selected {
            span("▎ ", Role::Accent)
        } else {
            span("  ", Role::Text)
        };
        let role = if selected { Role::Text } else { Role::Muted };
        vec![
            marker,
            span(format!("{title:<36}"), role),
            span(detail, Role::Faint),
        ]
    };
    let mut dock = dock("billing");
    dock.state = State::NeedsYou;
    dock.activity = "run cargo test -p billing in /workspace?".to_owned();
    dock.panel = vec![
        option(true, "deny", "withhold consent; pause an active goal"),
        option(
            false,
            "allow once",
            "one start; a failure does not refund it",
        ),
        option(
            false,
            "allow repeated checks",
            "this goal and this exact command only",
        ),
    ];
    dock.composer = keys(&[("enter", "apply"), ("↑↓", "choose"), ("esc", "back")]);
    Scene {
        blocks: vec![Block::Prompt("/goal check".to_owned())],
        sessions: idle_sessions("billing"),
        dock,
    }
}

const GALLERY_PROSE: &str =
    "First paragraph of prose.\n\nSecond paragraph of prose, directly after the first.";

fn markdown_gallery() -> Scene {
    let text = |value: &str| vec![span(value, Role::Text)];
    let marker = |mark: &str, value: &str| vec![span(mark, Role::Accent), span(value, Role::Text)];
    let blocks = vec![
        Block::Prompt("show me every markdown element".to_owned()),
        answer(GALLERY_PROSE),
        Block::Raw(vec![
            vec![bold("Level one heading", Role::Accent)],
            Vec::new(),
            vec![bold("Level two heading", Role::Text)],
            Vec::new(),
            vec![bold("Level three heading", Role::Muted)],
        ]),
        Block::Raw(vec![
            marker("1. ", "First numbered item"),
            marker(
                "2. ",
                "Second numbered item that is deliberately long enough to wrap onto a second row",
            ),
            text("   at this width"),
            marker("3. ", "Third numbered item"),
            marker("   ◦ ", "nested bullet under three"),
        ]),
        Block::Raw(vec![
            marker(
                "• ",
                "A bullet item that is also deliberately long enough to wrap onto a second physical",
            ),
            text("  row here"),
            marker("• ", "Short bullet"),
            marker("☐ ", "open task"),
            marker("☑ ", "done task"),
        ]),
        Block::Raw(vec![vec![
            span("▎ ", Role::Faint),
            span(
                "A quote long enough to wrap onto a second row so we can see whether the rail repeats.",
                Role::Muted,
            ),
        ]]),
        Block::Raw(vec![vec![
            span("Some ", Role::Text),
            span("emphasis", Role::Text),
            span(", some ", Role::Text),
            bold("strong", Role::Text),
            span(", some ", Role::Text),
            span("inline code", Role::Accent),
            span(", a ", Role::Text),
            span("link", Role::Accent),
            span(", a footnote[1], and math $x^2$.", Role::Text),
        ]]),
        Block::Raw(vec![vec![span("[1] The footnote body.", Role::Muted)]]),
        Block::Code {
            language: "rust".to_owned(),
            lines: [
                "fn main() {",
                "    let x = 1;",
                "    if x > 0 {",
                "        println!(\"{x}\");",
                "    }",
                "}",
            ]
            .map(str::to_owned)
            .to_vec(),
        },
        Block::Raw(vec![
            vec![bold("Role    Default", Role::Text)],
            vec![span("──────  ─────────", Role::Faint)],
            text("text    white"),
            text("muted   dark grey"),
        ]),
        Block::Raw(vec![vec![span("─".repeat(40), Role::Faint)]]),
        answer("Tail paragraph."),
    ];
    Scene {
        blocks,
        sessions: idle_sessions("markdown"),
        dock: dock("markdown"),
    }
}

fn working_sessions() -> Vec<SessionRow> {
    vec![
        session(
            State::NeedsYou,
            "deploy helper",
            0,
            false,
            "wants to run cargo publish",
            "$0.08",
        ),
        session(
            State::Running,
            "sse reconnect",
            3,
            true,
            "editing sse.rs",
            "$0.42",
        ),
        session(
            State::Idle,
            "refactor cli",
            0,
            false,
            "Done. Every file compiles.",
            "$1.10",
        ),
        session(
            State::Done,
            "release notes",
            0,
            false,
            "Drafted the checklist.",
            "$0.31",
        ),
    ]
}

fn golden_path() -> Scene {
    let mut failed = tool(
        State::Failed,
        "Ran",
        "cargo test -p qq-client reconnect",
        "exit 101",
    );
    failed.tail = vec!["thread 'reconnect' panicked at sse.rs:212: timed out".to_owned()];
    let mut edit = tool(State::Running, "Editing", "crates/qq-client/src/sse.rs", "");
    edit.diff = vec![
        DiffLine {
            number: 208,
            sign: ' ',
            text: "let mut clock = Clock::system();".to_owned(),
            changed: None,
        },
        DiffLine {
            number: 208,
            sign: '-',
            text: "tokio::time::sleep(Duration::from_millis(50)).await;".to_owned(),
            changed: Some("sleep(Duration::from_millis(50)).await".to_owned()),
        },
        DiffLine {
            number: 208,
            sign: '+',
            text: "tokio::time::pause();".to_owned(),
            changed: Some("pause()".to_owned()),
        },
    ];
    let mut dock = dock("sse reconnect");
    dock.state = State::Running;
    dock.activity = "editing sse.rs · 14s".to_owned();
    dock.attention = "◇ 1 needs you  ^G".to_owned();
    dock.compact_attention = "4 sessions  ◐ 1  ◇ 1  ^G".to_owned();
    dock.context = "31%".to_owned();
    dock.cost = "$0.42".to_owned();
    Scene {
        blocks: vec![
            Block::Prompt("make the sse reconnect test deterministic".to_owned()),
            Block::Tool(tool(State::Done, "Read", "crates/qq-client/src/sse.rs", "412 lines")),
            Block::Tool(failed),
            Block::Tool(edit),
            Block::Answer {
                text: "The test slept on wall-clock time; pausing the runtime clock removes the race. Applying the edit now.".to_owned(),
                streaming: true,
            },
        ],
        sessions: working_sessions(),
        dock,
    }
}

fn tools(opened: bool) -> Scene {
    let mut blocks = vec![Block::Prompt("clean up the renderer".to_owned())];
    if opened {
        blocks.push(Block::Receipt(
            "6 calls · read 2 · ran 2 · edited 2   2 files   ttft 0.4s   3s".to_owned(),
        ));
        for index in 0..6 {
            let row = match index % 3 {
                0 => {
                    let mut read = tool(
                        State::Done,
                        "Read",
                        &format!("crates/qq-tui/src/file_{index}.rs"),
                        "20 lines",
                    );
                    read.tail = vec!["fn main() {}".to_owned(); 3];
                    read
                }
                1 => tool(
                    State::Done,
                    "Edited",
                    &format!("crates/qq-tui/src/file_{index}.rs"),
                    "+1 −0",
                ),
                _ => {
                    let mut ran = tool(
                        State::Done,
                        "Ran",
                        &format!("cargo test -p crate_{index}"),
                        "exit 0",
                    );
                    ran.tail = vec!["test result: ok. 12 passed".to_owned()];
                    ran
                }
            };
            blocks.push(Block::Tool(row));
        }
    } else {
        blocks.push(Block::Receipt(
            "4 calls · read 2 · ran 2   2 files +2 −0   3s".to_owned(),
        ));
        blocks.push(Block::Tool(tool(
            State::Done,
            "Edited",
            "crates/qq-tui/src/file_1.rs",
            "+1 −0",
        )));
        blocks.push(Block::Tool(tool(
            State::Done,
            "Edited",
            "crates/qq-tui/src/file_4.rs",
            "+1 −0",
        )));
    }
    blocks.push(answer("Done. Every file compiles."));
    Scene {
        blocks,
        sessions: idle_sessions("renderer"),
        dock: dock("renderer"),
    }
}

fn wrap_diff() -> Vec<Row> {
    vec![
        vec![
            span("  40     ", Role::Faint),
            span("let mut column = 0;", Role::Text),
        ],
        vec![
            span("  41 -   ", Role::Remove),
            span("while column ", Role::Remove),
            super::inverse("<=", Role::Remove),
            span(" width {", Role::Remove),
        ],
        vec![
            span("  41 +   ", Role::Add),
            span("while column ", Role::Add),
            super::inverse("<", Role::Add),
            span(" width {", Role::Add),
        ],
    ]
}

fn approval_dock_with(title: &str, prompt: &str, preface: &str) -> Scene {
    let mut dock = dock(title);
    dock.state = State::NeedsYou;
    dock.activity = "apply this edit?".to_owned();
    dock.mode = Mode::Ask;
    dock.context = "18%".to_owned();
    dock.cost = "$0.12".to_owned();
    dock.panel = wrap_diff();
    dock.composer = keys(&[
        ("y", "apply"),
        ("n", "deny"),
        ("Y / N", "with a note"),
        ("e", "full diff"),
    ]);
    let mut blocks = Vec::new();
    if !prompt.is_empty() {
        blocks.push(Block::Prompt(prompt.to_owned()));
        blocks.push(answer(preface));
    }
    blocks.push(Block::Tool(tool(
        State::NeedsYou,
        "Edit",
        "crates/qq-tui/src/view/wrap.rs",
        "waiting",
    )));
    Scene {
        blocks,
        sessions: idle_sessions(title),
        dock,
    }
}

fn approval() -> Scene {
    approval_dock_with(
        "wrap fix",
        "fix the off-by-one in wrap.rs",
        "I'll patch the loop bound.",
    )
}

fn approval_dock() -> Scene {
    approval_dock_with("wrap fix", "", "")
}

fn reasoning() -> Scene {
    Scene {
        blocks: vec![
            Block::Prompt("why does the test flake?".to_owned()),
            Block::Reasoning("12s".to_owned()),
            answer(
                "The test slept on wall-clock time; pausing the runtime clock removes the race.",
            ),
        ],
        sessions: idle_sessions("flake"),
        dock: dock("flake"),
    }
}

fn steering() -> Scene {
    let mut dock = dock("steering");
    dock.state = State::Running;
    dock.activity = "thinking · 6s".to_owned();
    dock.queued = vec!["also check the tests".to_owned()];
    dock.composer = Composer::Draft("keep the public API".to_owned());
    Scene {
        blocks: vec![
            Block::Prompt("tidy the reconnect module".to_owned()),
            answer("the model turn"),
            Block::Prompt("prefer edit_file".to_owned()),
            Block::Raw(vec![vec![span("steered", Role::Faint)]]),
            Block::Prompt("never mind".to_owned()),
            Block::Raw(vec![vec![span(
                "not sent · the run finished first",
                Role::Faint,
            )]]),
        ],
        sessions: idle_sessions("steering"),
        dock,
    }
}

fn sessions() -> Scene {
    let mut dock = dock("release");
    dock.attention = "◇ 1 needs you  ^G".to_owned();
    dock.compact_attention = "8 sessions  ◐ 2  ◇ 2  ^G".to_owned();
    dock.model = "gpt-5.5".to_owned();
    Scene {
        blocks: vec![
            Block::Prompt("plan the release".to_owned()),
            answer("Drafted the checklist."),
            Block::Agents {
                lanes: vec![Lane {
                    state: State::NeedsYou,
                    title: "deploy helper".to_owned(),
                    activity: "wants to run rm -rf build".to_owned(),
                }],
            },
        ],
        sessions: vec![
            session(
                State::NeedsYou,
                "Refactor",
                0,
                false,
                "finished · unseen",
                "$0.12",
            ),
            session(
                State::Running,
                "Survey callers",
                0,
                false,
                "Found twelve call sites",
                "",
            ),
            session(
                State::Running,
                "Write tests",
                0,
                false,
                "compacting context",
                "",
            ),
            session(State::Idle, "release", 1, true, "◇ deploy helper", ""),
            session(State::Idle, "Scratch", 0, false, "", ""),
            session(State::Idle, "Notes", 0, false, "", ""),
            session(State::Done, "Migrate", 0, false, "done", "$0.04"),
        ],
        dock,
    }
}

const FANOUT_TITLES: [&str; 20] = [
    "audit parser",
    "deploy helper",
    "docs pass",
    "bench",
    "lint sweep",
    "port sse",
    "trim deps",
    "fix flake",
    "wire config",
    "cache rail",
    "cut header",
    "theme faint",
    "dock rule",
    "receipts",
    "lanes",
    "copy model",
    "osc 52",
    "switcher",
    "ranking",
    "release check",
];

fn fanout() -> Scene {
    let lanes: Vec<Lane> = FANOUT_TITLES
        .iter()
        .enumerate()
        .map(|(index, title)| {
            let (state, activity) = match index {
                1 => (
                    State::NeedsYou,
                    "wants to run cargo publish --dry-run".to_owned(),
                ),
                3 | 9 | 15 => (State::Done, "done · 3 files".to_owned()),
                7 => (State::Failed, "exit 101 in reconnect".to_owned()),
                _ => (
                    State::Running,
                    format!("reading src/{}.rs", title.replace(' ', "_")),
                ),
            };
            Lane {
                state,
                title: (*title).to_owned(),
                activity,
            }
        })
        .collect();
    let mut dock = dock("v0.1.11");
    dock.state = State::Running;
    dock.activity = "16 agents working · 2m 10s".to_owned();
    dock.attention = "◇ 2 need you  ^G".to_owned();
    dock.compact_attention = "3 sessions  ◐ 16  ◇ 2  ^G".to_owned();
    dock.context = "22%".to_owned();
    dock.cost = "$3.40".to_owned();
    Scene {
        blocks: vec![
            Block::Prompt("split the v0.1.11 plan into twenty agents".to_owned()),
            Block::Agents { lanes },
        ],
        sessions: vec![
            session(
                State::NeedsYou,
                "deploy helper",
                0,
                false,
                "wants to run cargo publish",
                "v0.1.11 ›",
            ),
            session(
                State::NeedsYou,
                "fix flake",
                0,
                false,
                "exit 101 in reconnect",
                "v0.1.11 ›",
            ),
            session(
                State::Running,
                "v0.1.11",
                20,
                true,
                "◐ 16  ◇ 1  ✕ 1  ● 3",
                "$3.40",
            ),
            session(State::Idle, "refactor cli", 0, false, "", "$1.10"),
            session(State::Done, "release notes", 0, false, "", "$0.31"),
        ],
        dock,
    }
}

const SWITCHER_ROWS: [(State, &str, usize, &str, &str); 4] = [
    (
        State::NeedsYou,
        "deploy helper",
        0,
        "2m",
        "wants to run cargo publish --dry-run",
    ),
    (
        State::Running,
        "sse reconnect",
        3,
        "now",
        "editing crates/qq-client/src/sse.rs",
    ),
    (
        State::Idle,
        "refactor cli",
        0,
        "1h",
        "Done. Every file compiles.",
    ),
    (
        State::Done,
        "release notes",
        0,
        "3h",
        "Drafted the checklist.",
    ),
];

fn switcher() -> Scene {
    let mut scene = golden_path();
    scene.dock.state = State::Idle;
    scene.dock.activity = "switch".to_owned();
    scene.dock.attention = "30 sessions".to_owned();
    scene.dock.compact_attention = String::new();
    scene.dock.panel = SWITCHER_ROWS
        .iter()
        .enumerate()
        .map(|(index, (state, title, agents, age, last))| {
            let marker = if index == 1 {
                span("▎ ", Role::Accent)
            } else {
                span("  ", Role::Text)
            };
            let badge = if *agents > 0 {
                format!("↳{agents}")
            } else {
                String::new()
            };
            vec![
                marker,
                span(format!("{} ", state.glyph()), state.row_role()),
                span(
                    format!("{title:<18}"),
                    if index == 1 { Role::Text } else { Role::Muted },
                ),
                span(format!("{badge:<4}"), Role::Faint),
                span(format!("{age:>4}   "), Role::Faint),
                span(*last, Role::Muted),
            ]
        })
        .collect();
    scene.dock.composer = Composer::Draft("sse".to_owned());
    scene.dock.hints = Some(vec![
        span("enter", Role::Text),
        span(" open · ", Role::Faint),
        span("tab", Role::Text),
        span(" agents · ", Role::Faint),
        span("^A", Role::Text),
        span(" all agents · ", Role::Faint),
        span("^D", Role::Text),
        span(" delete · ", Role::Faint),
        span("esc", Role::Text),
        span(" back", Role::Faint),
    ]);
    scene
}

fn first_frame() -> Scene {
    let hint = |key: &str, label: &str| {
        [
            span(key, Role::Text),
            span(format!("  {label}    "), Role::Faint),
        ]
    };
    let mut hints = Vec::new();
    for (key, label) in [
        ("/models", "choose a model"),
        ("@path", "mention a file"),
        ("^T", "switch session"),
        ("?", "help"),
    ] {
        hints.extend(hint(key, label));
    }
    let mut dock = dock("new session");
    dock.context = String::new();
    Scene {
        blocks: vec![Block::Banner(vec![
            vec![
                bold("qq 0.1.11", Role::Text),
                span(" · ~/qq  main · gpt-5.5 high", Role::Muted),
            ],
            Vec::new(),
            hints,
        ])],
        sessions: Vec::new(),
        dock,
    }
}
