# qq
A composable toolkit for building, running, and orchestrating AI agents.

Running `qq` with no subcommand opens the interactive TUI against a
user-scoped background server (`qq serve` runs one in the foreground).
Agents read, search, and edit workspace files, run shell commands, and
call MCP tools — every mutating action gated by an approval policy.
Design documentation lives in `docs/` (`design/` for decisions,
`plans/` for proposals).

## Quick Start

Set `OPENAI_API_KEY` and `QQ_MODEL`, then stream one response:

```sh
cargo run -- ask "Reply with pong"
```

To use a ChatGPT Codex subscription instead of an API key, sign in through the
browser and select an `openai-codex` model:

```sh
cargo run -- auth login openai-codex
QQ_MODEL=openai-codex/MODEL cargo run -- ask "Reply with pong"
```

On Windows, credentials that exceed Credential Manager's per-entry limit are
stored in a DPAPI-encrypted file bound to the current Windows user and machine.
Moving that file to another account or computer requires signing in again.

To use xAI, set `XAI_API_KEY` or sign in with OAuth. OAuth credentials are
refreshed and stored under the selected profile:

```sh
QQ_MODEL=xai/grok-4.3 cargo run -- ask "Reply with pong"
cargo run -- auth login xai --oauth
```

## Build Profiles

The default build is the full profile: every supported provider family,
including Amazon Bedrock and Bedrock Mantle. Embedders that only need the HTTP
provider families (OpenAI, Anthropic, Google, and compatible endpoints) can
build the minimal profile, which drops the AWS SDK dependency closure:

```sh
cargo build --release --no-default-features
```

A minimal build still accepts Bedrock configuration but refuses to compile a
Bedrock provider with a configuration error naming the missing
`provider-bedrock` feature. Library embedders select the same feature on
`qq-provider` directly.

## Amazon Bedrock Mantle

Mantle reuses the OpenAI Responses, OpenAI Chat Completions, and Anthropic
Messages wire protocols. Configure a regional deployment with the standard AWS
credential chain:

```ron
(
    version: 1,
    model: "bedrock-mantle/MODEL",
    providers: {
        "bedrock-mantle": AmazonBedrockMantle(
            region: "us-east-1",
            api: OpenAiResponses,
            auth: Aws(DefaultChain),
        ),
    },
)
```

`api` also accepts `OpenAiChatCompletions` and `AnthropicMessages`. Authentication
may use `Aws(Profile("PROFILE"))` or a region-bound API key such as
`ApiKey(Env("BEDROCK_MANTLE_API_KEY"))`.

Profiles that use `credential_process` are currently unsupported and rejected.
QQ disables that aws-config provider because it cannot guarantee termination of
the subprocess when credential loading times out.

## Google Gemini

Set `GEMINI_API_KEY` and select a model under the built-in `google` provider:

```sh
QQ_MODEL=google/gemini-2.5-flash cargo run -- ask "Reply with pong"
```

Google API keys are sent only in the sensitive `x-goog-api-key` header, never in
the request URL.

## Tools And Approvals

Sessions run under an approval mode: `read-only` (only read-only tools
execute), `ask` (edits, writes, shell, and MCP calls each request
approval), or `auto` (workspace-contained edits and allowlisted shell
prefixes run unprompted). Approval prompts show the exact command or an
edit diff, and "approve for session" records a grant — shell grants are
command prefixes matched at word granularity. Nonzero exits and denials
return to the model as tool errors, not run failures. See
`docs/design/tools.md` for the full policy design.

Headless runs answer approvals themselves: `qq run --approval read-only`
denies every held call, `auto` denies only what the policy escalated (dangerous
shell), and `full` approves everything. Between `auto` and `full`,
`--allow-tool <name>` and `--allow-shell "<prefix>"` (both repeatable) approve
a held call for the session with the same word-boundary prefix rule the
interactive "approve for session" uses, so `--allow-shell "cargo test"` covers
`cargo test -p qq-core` and never `cargo test | sh`. `--steer-stdin` reads one
steering message per stdin line and injects each at the run's next model/tool
boundary; without it stdin is left alone.

Use `qq run --profile <name>` to select a profile's tool catalog. Optional
`policy.exposed_tools: ["read_file", "search"]` narrows it by intersection
across configuration layers; an empty list exposes no tools. `--allow-tool`
and `--allow-shell` grant held calls and cannot restore tools excluded from
the catalog. See the [headless contract](docs/design/headless-contract.md).

## MCP Servers

MCP servers are declared in configuration and their tools join the same
approval and event flow as built-ins, namespaced `mcp__<server>__<tool>`:

```ron
mcp: {
    "executor": Stdio(command: "executor", args: ["mcp"], eager: true,
                      allow: ["execute", "skills", "resume"]),
}
```

This local example uses Executor's official CLI. Stdio and streamable-HTTP
transports are supported; declarations are trust-gated in workspace
configuration, and authenticated HTTP endpoints use `Env(...)` or `Stored(...)`
bearer references rather than literal secrets.

## TUI Configuration

TUI preferences use a separate `tui.ron` document. QQ loads compiled defaults,
then the global configuration directory's `tui.ron`, then `.qq/tui.ron` files
from the repository root to the current directory.

```ron
(
    version: 1,
    theme: "qq",
    bindings: (
        toggle_navigator: ["Ctrl-T"],
        create_root_session: ["Alt-N"],
        create_child_session: ["Alt-C"],
        cancel_run: ["Ctrl-X"],
        interrupt_run: ["Alt-S"],
    ),
)
```

An omitted action inherits the previous layer. An empty list disables that
action. Invalid chords and collisions are rejected before the TUI starts.

Every other key lives in one command table. `?` on an empty composer, `F1`, or
`/help` lists every command with its chord and slash name grouped by area;
`Ctrl-K` or `/commands` opens the same list as a searchable palette that runs
the highlighted command on Enter. Rebinding an action updates every hint that
mentions it. The mouse wheel scrolls the transcript; `PageUp`/`PageDown` and
`Shift-Up`/`Shift-Down` scroll from the keyboard and `Ctrl-Home`/`Ctrl-End`
jump to the top and the live tail. Hold Shift to select text with the mouse, or
`/mouse` to hand the mouse back to the terminal. `Ctrl-R` searches the session's
prompt history.

Each tool call is one row: `● Edit  src/sse.rs  +12 −3  1.2s`, with a spinner
and live elapsed time while it runs. `Ctrl-Up`/`Ctrl-Down` select a call and
Enter expands it alone: a read or search shows the head of its result, an edit
its diff with line numbers, a command the tail of its output; MCP tools list
their arguments. Expanded rows also show when the call started, finished or how
long it has run, and when it last produced output. Enter on a `spawn_agent` row
opens the child instead. `Ctrl-O` folds quiet finished blocks to one summary
row for reading back a long transcript, and `Alt-R` toggles reasoning. The
rule above the composer shows the running activity, elapsed time, and time to
first token, or the latest notice; the key hints for the current state sit at
its right.

With several agents, a sidebar groups sessions by what you should do about
them (NEEDS YOU, WORKING, IDLE, DONE) with unread counts. It appears on its
own at 100 columns or more and takes a quarter of the width up to 28 columns;
`Ctrl-\` toggles it, and below that width a one-row agent strip above the
composer carries the same counts. `Ctrl-G` jumps
to the next session that needs you; `Alt-A`/`Alt-D` approve or deny a call
waiting in another session without leaving the current one. In an approval,
`Shift-Y`/`Shift-N` decide and then steer the run with a note. `/attention`
lists everything that needs you across the workspace and `/changes` shows every
file agents edited, flagging files touched by more than one. Esc returns
from either to the transcript.

`theme` names a color theme. QQ ships `qq` (follows your terminal palette),
`ink` and `ember` (its own), and ports of gruvbox, tokyonight, catppuccin,
dracula, nord, solarized, onedark, rose-pine, kanagawa, everforest, and
monokai; `qq config explain tui.theme` lists them. A `<name>.ron` file under
the global configuration directory's `themes/` or a project's `.qq/themes/`
adds a theme or shadows a shipped one. `/theme` opens a picker that previews each theme live (Enter
keeps it for the session, Esc restores); the notice it leaves shows the line to
add to `tui.ron`. See `docs/design/theme.md` for the document shape.

One session is on screen at a time; the composer, approvals, and footer follow
it. To watch two sessions side by side, run two `qq` clients in your terminal
multiplexer against the same workspace. When the terminal is unfocused, an approval request or a finished run rings
the terminal bell and posts an OSC 9 desktop notification where supported.

While a run is executing, Enter steers it: the draft joins the run at its
next model/tool boundary and appears in the transcript as a `steering` row
until it is applied. `Alt-S` interrupts first (the in-flight model turn or
tool is aborted, partial text stands) and then steers, for when the run is
heading the wrong way right now. `Ctrl-Enter` instead holds the draft locally
until the run finishes and sends it as the next prompt; `Alt-Up` pulls the
newest held draft back for editing. `Esc Esc` cancels the run. Steering is
offered only when the server advertises it; otherwise Enter holds the draft.

The interactive composer recognizes `/help`, `/commands`, `/models`, `/profile`,
`/approval`, `/skills`, `/theme`, `/new`, `/sessions` (also `/resume`), `/agents`, `/prune`, `/mouse`,
`/attention`, `/changes`, `/editor`, `/compact`, `/rollback`, and `/quit` (also `/exit`). Typing after the slash filters by subsequence, so `/mdl` finds
`/models`. `/compact`
summarizes an idle session's history into a compact context so long
sessions keep going, and the notice quotes the start of the summary the model
wrote; `/rollback` discards the newest compaction of an idle session and
restores the history beneath it. Stale read-only tool results are also pruned
from model context automatically. `/models` applies the choice to the
focused session (or creates one when none is focused); Ctrl-N always creates a
new session with the selected model. The pick also becomes the client default
for later `/new` creates until you choose another model. The picker only lists
built-in providers with an available credential, and the footer shows context
usage, the selected model, working directory, and focused session cost. Slash
command suggestions run immediately when selected with Enter or Tab. QQ names a
session from its first prompt; `/sessions` supports typing to search those names
before selecting one with Enter. In the session picker, Ctrl-D deletes the
highlighted session (with confirmation; a session with an active run must be
cancelled first). `/prune` asks before deleting every empty session in the
workspace.

`/profile` lists the agent profiles the server advertises for the workspace:
those under `profiles` in `.qq/config.ron` and those declared by trusted agent
packs under `.qq/packs/`, each with its approval mode, model override, and
declaring pack. Enter applies the profile to the focused idle session (a running
session must finish first) or, with nothing focused, makes it the default for
sessions created next. The top row shows `as <profile>` whenever the profile in
effect is not `default`. `qq run --profile <name>` selects a profile for a
headless run and fails before the run starts if the name is unknown.

`/approval` lists the approval modes the server accepts (`read_only`, `ask`,
`auto`, `full`) with what each holds for approval. Enter applies the mode to the
focused session — it takes effect at the next held tool call, so a running
session may change too — or, with nothing focused, sets the mode new sessions
are created with. The top row names the mode in effect whenever it is not
`auto`.

Slash completion also lists the workspace's own commands (`.qq/commands/*.md`)
and skills (`.qq/skills/<name>/SKILL.md`, plus those from trusted packs) after
the client commands, with their descriptions. Accepting a command leaves
`/name ` in the composer for its arguments; accepting a skill submits `/name`
so the runtime loads it into the run. `/skills` opens the same index as a
picker grouped by kind with each document's source, marking documents the
model may not load on its own as `explicit only`.
