# Configuration reference

QQ reads [RON](https://github.com/ron-rs/ron) documents. Every file starts
with `(` and `version: 1,` and ends with `)`. Unknown keys are errors, so a
typo cannot silently do nothing. Files are limited to 1 MiB.

```ron
(
    version: 1,
    model: "anthropic/claude-sonnet-5",
    reasoning_effort: medium,
)
```

`qq init` writes a commented starter file with the model you choose (or
`--model PROVIDER/MODEL`); `qq init --project` writes `.qq/config.ron` in
the current directory instead. Neither replaces an existing file without
`--force`. `qq config check` validates the merged result; `qq config show`
prints it with secrets redacted; `qq config explain model` says which file
set a value; `qq config sources` lists every file consulted in order.

## Files and precedence

Later layers win. Maps (`providers`, `mcp`, `profiles`, `packs`) merge by
key; an entry set to `Remove` deletes what an earlier layer declared.
Sections `delegation` and `audit` replace as a whole. The `goals` section
merges individual fields, including nested `per_run` fields; `goals: Clear`
restores compiled defaults under managed ceilings.

| Order | Layer | Path |
| ---: | --- | --- |
| 1 | compiled defaults | built-in providers and policy |
| 2 | global packs | `<global>/packs/<id>/pack.ron` |
| 3 | organization manifest | cached from `qq org enroll` |
| 4 | **your global config** | `<global>/config.ron`, then `<global>/config.d/*.ron` sorted |
| 5 | project layers, repository root first, current directory last | per directory: `.qq/packs/<id>/pack.ron` (each manifest trusted on its own), `qq.ron`, `.qq/config.ron`, `.qq/config.d/*.ron` |
| 6 | explicit file | `QQ_CONFIG=/path/to/file.ron` |
| 7 | inline document | `QQ_CONFIG_CONTENT='(version: 1, …)'` |
| 8 | overrides | `--model` / `QQ_MODEL`, `--organization` / `QQ_ORGANIZATION`, `--max-output-tokens` |
| 9 | managed | `/etc/qq/managed.ron` + `managed.d/` (Linux), `/Library/Application Support/qq/` (macOS), `%ProgramData%\qq\` (Windows); must be root-owned |
| 10 | MDM | macOS forced preference `dev.qq` / `ManagedConfig`; Windows policy `HKLM\Software\Policies\dev.qq\ManagedConfig` (`REG_SZ`); none on Linux |

`<global>` is `~/.config/qq` on Linux, `~/Library/Application
Support/dev.qq.qq` on macOS, `%APPDATA%\qq\qq\config` on Windows;
`qq config paths` prints it, along with the global `config.ron`, `tui.ron`,
and the data, managed, and organization paths, each marked `(exists)` or
`(missing)`. The global `config.ron` may be a symlink to a regular file;
project files may not.

Set `QQ_HOME` to an absolute directory to keep a separate QQ instance's
user configuration in `<QQ_HOME>/config` and its sessions, trust records and
credential metadata in `<QQ_HOME>/data`. Diagnostic logs also use
`<QQ_HOME>/data/logs`; server discovery and coordination use
`<QQ_HOME>/data/runtime`, so separate homes do not attach to each other's server.
This is useful for isolated instances and testing; it
does not change the workspace, managed policy, OS keyring or what shell
commands can reach.
An empty or relative `QQ_HOME` is an error. Without it, platform paths stay
unchanged.

Project layers that declare anything sensitive — `model`, `providers`,
`mcp`, `packs`, `profiles`, `delegation`, `audit`, `goals`,
`reasoning_effort`, `organization`, or `worker_model` — are loaded only after `qq trust`
has accepted that exact file content. See
[Permissions and trust](permissions.md#project-trust).

Fragments in `config.d/` are a good place for machine-local settings a
repository should not commit; this repository's `.gitignore` excludes
`.qq/config.d/*-local.ron`.

## Top-level keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `version` | `1` | required | document schema |
| `model` | `"PROVIDER/MODEL"` | none; required to run | the route sessions start with |
| `worker_model` | `"PROVIDER/MODEL"` | none | deprecated; use `delegation.roster` |
| `organization` | string | none | which enrolled organization manifest applies (`qq org`) |
| `max_output_tokens` | integer | `16384` | cap on generated tokens per model turn; a model's own limit applies if lower. One exception: when a turn is cut at this cap with nothing visible (all hidden reasoning) or right after a complete tool call, that one retry is sent with the cap doubled, up to the model's limit and never past a managed `policy.max_output_tokens`, at most once per run. Use `policy.max_output_tokens` for a hard ceiling; run budgets still bound spend |
| `reasoning_effort` | `none` `minimal` `low` `medium` `high` `xhigh` `max` `default` | provider default | effort hint for reasoning models that accept one. `default` lets the provider choose. Any other value outside the model's known accepted set (catalog or discovery) fails at plan time naming the accepted ones; with no known set, a rejected value fails at the provider ([providers](providers.md)) |
| `approval_timeout_seconds` | `1`–`86400` | absent | server-side bound on how long a held call waits for you before it is denied. Absent is **no deadline**: the prompt waits for you, the run's own deadline, or cancellation. Not trust-gated: it can only shorten a wait. `qq serve` and `qq daemon` read it from global and managed configuration only; `qq run` and the TUI also read the project's |
| `providers` | map | built-ins | provider declarations; [below](#providers) |
| `mcp` | map | empty | MCP servers; [MCP servers](mcp.md) |
| `profiles` | map | empty | named per-session presets; [below](#profiles) |
| `packs` | map | discovered | agent packs by id; [below](#packs) |
| `delegation` | section | empty | sub-agent roster and bounds; [below](#delegation) |
| `audit` | section | `off` | final-answer audit; [below](#audit) |
| `goals` | section | finite defaults | defaults and bounds for durable goals; [below](#goals) |
| `policy` | section | see below | the tool catalog, and what may never run; [below](#policy) |

### Model routes

`PROVIDER/MODEL`. The provider must exist (built-in or declared) and the
model must be either in QQ's catalog for that provider or declared under the
provider's `models`. `qq ask --model x/y hi` fails fast with the available
routes when the pair is unknown.

Built-in catalog routes are listed in
[Providers](providers.md#built-in-models).

## `providers`

Built-in providers exist without any declaration; declare one only to
change its credential reference or add models. Custom endpoints need a
declaration.

```ron
providers: {
    // Change where a built-in reads its key.
    "openai": OpenAi(api_key: Env("MY_OPENAI_KEY")),

    // Codex subscription under a named credential profile.
    "openai-codex": OpenAiCodex(profile: "work"),

    // Any OpenAI- or Anthropic-compatible gateway.
    "gateway": Custom(
        connection: (
            base_url: "https://llm.example.com/v1",
            api: OpenAiChatCompletions,         // OpenAiResponses | OpenAiChatCompletions | AnthropicMessages | GoogleGenerateContent
            auth: Bearer(Stored("gateway/default")),  // NoAuth | ApiKey(ref) | Bearer(ref) | Header("X-Name", ref)
            headers: { "X-Team": "platform" },  // static, non-secret headers
        ),
        models: {
            "big": (name: "Big model", context_window: 200000, max_output_tokens: 32000, reasoning: true),
        },
    ),

    // LiteLLM: same shape as Custom, LiteLLM-aware.
    "litellm": LiteLlm(connection: (base_url: "…", api: OpenAiChatCompletions, auth: ApiKey(Env("LITELLM_API_KEY")))),

    // Amazon Bedrock (Converse API) and Bedrock Mantle (OpenAI/Anthropic wire).
    "bedrock": AmazonBedrock(region: "us-east-1", auth: Aws(DefaultChain)),
    "bedrock-mantle": AmazonBedrockMantle(region: "us-east-1", api: AnthropicMessages, auth: Aws(Profile("dev"))),

    // Drop a provider an earlier layer declared.
    "unused": Remove,
}
```

Provider kinds: `OpenAi`, `OpenAiCodex`, `Anthropic`, `Google`, `XAi`,
`LiteLlm`, `AmazonBedrock`, `AmazonBedrockMantle`, `Custom`.

### Secret references

Everywhere a credential is expected:

| Form | Meaning |
| --- | --- |
| `Env("NAME")` | read the environment variable at run time |
| `Stored("name")` | read the OS keyring entry created by `qq auth login` or `qq auth set name` |
| `Value("literal")` | a literal secret; rejected unless `policy.allow_literal_secrets: true` — do not commit these |

`Stored(...)` names resolve on *this machine only*. A repository's committed
config should reference `Env(...)`, or leave the credential to the
built-in default (`qq auth login PROVIDER` stores `PROVIDER/default`).

### `models` entries

Per model under a provider (all optional):

| Key | Meaning |
| --- | --- |
| `name` | display name |
| `api` | override the wire API for this model |
| `reasoning` | `true` if the model accepts `reasoning_effort` |
| `reasoning_efforts` | which efforts it accepts |
| `input` | `[Text]` or `[Text, Image]` |
| `context_window`, `max_output_tokens` | limits QQ uses for compaction and budgeting |
| `pricing` | `(input_usd_nanos_per_token, output_usd_nanos_per_token, cache_read_…, cache_write_…, provenance: "…")` so `--max-cost-usd` and the cost footer work |

`"model-id": Remove` hides a catalog model.

## `policy`

Which tools the model sees, and what may never run.

Any layer (your config, a trusted project):

```ron
policy: (
    builtin_preference: hint,             // off | hint | strict
    exposed_tools: ["read_file", "search", "edit_file", "shell"],
)
```

Administrators only, in `managed.ron` or MDM (anywhere else is an error):

```ron managed.ron
policy: (
    allowed_providers: ["anthropic", "openai"],
    denied_providers: ["xai"],
    max_output_tokens: 32000,
    require_https: true,
    allow_custom_providers: true,
    allow_literal_secrets: false,
    deny_tools: ["write_file"],
    deny_shell_prefixes: ["git push"],
    deny_hosts: ["*.internal.example.com"],
)
```

| Key | Who may set it | Meaning |
| --- | --- | --- |
| `builtin_preference` | any; only tightens | how hard the model is steered from shell habits to built-in tools |
| `exposed_tools` | any; intersects | the tool catalog; empty list exposes nothing |
| `allowed_providers` / `denied_providers` | managed layers only | which providers a model route may use; denies accumulate |
| `max_output_tokens` | managed layers only | ceiling for the top-level key; only lowers |
| `require_https` | managed layers only | reject `http://` custom endpoints (loopback exempt); default `true` |
| `allow_custom_providers` | managed layers only | allow `Custom`/`LiteLlm` declarations; default `true` |
| `allow_literal_secrets` | managed layers only | allow `Value(...)`; default `false` |
| `deny_tools`, `deny_shell_prefixes`, `deny_hosts` | managed layers only | `deny_shell_prefixes` (word-boundary prefixes) and `deny_hosts` (exact names or `*.suffix`, for `fetch`) are refused in every approval mode, Full included. `deny_tools` removes tools from side questions, and `shell` in it refuses goal checks |
| `goals` | managed layers only | goal ceilings; fieldwise minimum across administrators, retained by `goals: Clear` |

"Managed layers" are `managed.ron`, `managed.d/`, and MDM (rows 9 and 10
above). Setting a `managed layers only` key anywhere else fails with
`managed-only policy settings are only allowed in managed configuration`;
to cap output tokens for yourself, set the top-level `max_output_tokens`
instead. How approval modes work is in
[Permissions and trust](permissions.md).

## `profiles`

A profile is a named bundle of per-session defaults, selected with
`/profile` in the TUI or `qq run --profile NAME`. `default` is the top-level
configuration and cannot be declared. Names: 1–64 lowercase letters, digits,
hyphens.

```ron
profiles: {
    "cheap": Profile(model: "openai/gpt-5.4-mini", reasoning_effort: low),
    "careful": Profile(approval_mode: ask, max_output_tokens: 8000),
    "old": Remove,
}
```

Keys: `model`, `organization`, `max_output_tokens`, `approval_mode`
(`read_only` `ask` `auto` `full`), `reasoning_effort`. Pack
profiles (below) add prompts, skills, and tool filters.

## `packs`

A pack is a directory with `pack.ron` that bundles profiles, a persona
prompt, skills, commands, and MCP declarations. Packs are discovered from
`<global>/packs/<id>/` and `.qq/packs/<id>/`, or declared explicitly. A
project pack (discovered under `.qq/packs/` or named by a project file) loads
only once you have trusted that exact `pack.ron`; editing it disables the
pack until you run `qq trust` again. A
project file's `path` must stay inside the repository (no path that leads
out of it, and no symbolic link):

```ron
packs: {
    "reviewer": Pack(path: "tools/qq-packs/reviewer"),   // relative to this file
    "legacy": Remove,
}
```

`pack.ron`:

```ron pack.ron
(
    schema: 1,
    id: "reviewer",
    version: "2026.09",                  // the pack's own version; any 1–64 bytes
    name: "Code reviewer",
    requires: (protocol: 14),
    profiles: {
        "reviewer": (
            approval_mode: read_only,
            prompt: "prompts/persona.md",
            skills: ["skills"],
            commands: ["commands"],
            tools: (deny: ["shell", "write_file", "edit_file", "spawn_agent", "mcp__*"]),
            mcp: [],                       // subset of this pack's mcp; absent = all
        ),
    },
    mcp: {},                               // same shape as config `mcp`
)
```

Limits: 32 entries across `packs/` directories per load (including
directories without a `pack.ron` and stray files), 16 profiles per pack, 64 KiB manifest. A pack profile shadows
nothing: a profile of the same name in your config wins.

## `delegation`

Which models a run may spawn sub-agents on and how deep.

```ron
delegation: (
    roster: [
        (route: "openai/gpt-5.4-mini", role: fast, note: "lookups and summaries"),
        (route: "anthropic/claude-sonnet-5", role: balanced),
        (route: "anthropic/claude-opus-4-8", role: strong, note: "hard reasoning"),
    ],
    default_role: balanced,      // fast | balanced | strong
    max_depth: 2,                // ≤ 3
    write_children: false,       // may children edit files?
)
```

Sub-agents are read-only by default. To let the spawning agent choose write
access, set `delegation.write_children: true`.
The `spawn_agent` tool then offers `authority: "read" | "write"`; omitting it
still chooses read-only. Write children use Auto to edit files and run commands
within the parent's authority. Only a
top-level run may spawn a write child, and only one write child runs at a time.
The parent's approval policy still gates the write spawn.

A run that may spawn sub-agents also gets `wait_agents` (wait for its
background sub-agents) and `cancel_agent` (stop one). They come and go with
`spawn_agent`: denying or exposing `spawn_agent` in policy, or allowing or
denying it in a pack, applies to all three, and the two are not tool names of their own there.

Up to 8 roster entries. Each route must resolve like `model`. An entry may pin
the reasoning effort its children run at with `effort: low` (any effort value);
without it, `fast` children run at `low` and `balanced` at `medium`, never above
the parent's own effort, fitted to what the route's catalog entry advertises
(a model without reasoning is sent none), while `strong` children inherit the
parent's. The deprecated `worker_model` counts as a `balanced` entry.
Design: [`../design/architecture.md`](../design/architecture.md)
(§ Concurrency And Multiple Agents and the `spawn_agent` description under
§ Structured Input And Steering).

## `goals`

Goal configuration is expressed in RON:

```ron
goals: (
    default_deadline_seconds: 86400,
    max_deadline_seconds: 604800,
    default_max_runs: 200,
    max_runs: 2000,
    per_run: (max_model_turns: 256),
    max_backoff_seconds: 1800,
    check_timeout_seconds: 600,
    approval_wait_seconds: 600,
),
```

Durations and limits must be positive. Deadline duration is at most seven
days, runs at most 2000, check timeout at most 3600 seconds, and backoff and
approval wait at most 86400 seconds. Defaults are clamped to effective
ceilings. `per_run` additionally accepts `max_duration_ms`, `max_tool_calls`,
`max_total_tokens`, `max_cost_usd_nanos`, `max_input_tokens`,
`max_output_tokens`, `max_tool_output_bytes`, `max_children` (at most 8),
and `max_concurrent_children` (at most 3). Omitted run limits inherit;
`goals: Clear` restores the finite compiled model-turn cap.

Administrators use `policy.goals` with `max_deadline_seconds`, `max_runs`,
`max_backoff_seconds`, `max_check_timeout_seconds`,
`max_approval_wait_seconds`, and the same `per_run` limits. These fields
combine by minimum, including per-run ceilings. An ordinary layer cannot
declare them or clear them. `qq config show` prints effective defaults and
ceilings; `qq config explain goals.default_max_runs` and
`qq config explain policy.goals.max_runs` identify their declaring sources.

Goal-bound work rechecks current workspace ceilings before starting model work
or a completion check. A lower ceiling or unavailable configuration pauses the
goal for configuration review; resume validates it again. Provider retries use
the accepted backoff cap, retaining any later model-requested wait. Unanswered
tool approvals pause the goal after `approval_wait_seconds`, or sooner at the
goal deadline or an ordinary approval timeout.

## `audit`

A second agent run that reviews the first's final answer.

```ron
audit: (
    mode: heuristic,      // off (default) | heuristic | always
    max_revisions: 1,     // ≤ 2
    role: strong,         // roster role that performs the audit
)
```

`heuristic` audits when the run edited files, ran a non-read command, made
twelve or more tool calls, or spawned a child.

## `tui.ron`

Terminal preferences live in a separate document, loaded from
`<global>/tui.ron` then `.qq/tui.ron` root-to-leaf.

```ron tui.ron
(
    version: 1,
    theme: "ink",           // qq | ink | ember | gruvbox | tokyonight | catppuccin | dracula | nord | solarized | onedark | rose-pine | kanagawa | everforest | monokai | <your-theme>
    bindings: (
        toggle_navigator: ["Ctrl-T"],
        create_root_session: ["Alt-N"],
        create_child_session: ["Alt-C"],
        cancel_run: ["Ctrl-X"],
        interrupt_run: ["Alt-S"],
    ),
)
```

An omitted binding inherits the layer below; an empty list disables the
action. Collisions are rejected before the TUI starts. `theme` defaults to
`ink` when the terminal advertises truecolor and to `terminal` otherwise.
Themes are `.ron` files in `<global>/themes/` or `.qq/themes/`; shape in
[`../design/theme.md`](../design/theme.md).

## Environment variables

| Variable | Effect |
| --- | --- |
| `QQ_HOME` | absolute root for separate user configuration and application data; managed policy and OS keyring remain unchanged |
| `QQ_MODEL` | override `model` for this process |
| `QQ_ORGANIZATION` | override `organization` |
| `QQ_CONFIG` | an extra config file applied after project layers |
| `QQ_CONFIG_CONTENT` | an inline RON document applied after `QQ_CONFIG` |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` or `GOOGLE_API_KEY`, `XAI_API_KEY` | built-in provider credentials when nothing is stored (`GEMINI_API_KEY` wins over `GOOGLE_API_KEY`) |
| `AWS_PROFILE`, `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`, `AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN`, `AWS_CONTAINER_CREDENTIALS_*` | Bedrock default credential chain |
| `COLORTERM` | `truecolor` / `24bit` selects the `ink` default theme |
| `EDITOR` | `/editor` in the TUI |

## Workspace commands and skills

Not configuration keys, but files QQ discovers:

| Path | What |
| --- | --- |
| `.qq/commands/<name>.md` | a slash command the composer offers as `/name`; the file is the prompt |
| `.qq/skills/<name>/SKILL.md` | a skill the model may load; `/name` loads it explicitly |
| `AGENTS.md` | project instructions the agent reads; see the [FAQ](faq.md#agents-md) |

## A complete example

```ron
(
    version: 1,
    model: "anthropic/claude-sonnet-5",
    max_output_tokens: 16384,
    reasoning_effort: medium,
    policy: (
        builtin_preference: hint,
    ),
    profiles: {
        "quick": Profile(model: "anthropic/claude-haiku-4-5", reasoning_effort: low),
    },
    delegation: (
        roster: [
            (route: "anthropic/claude-haiku-4-5", role: fast),
            (route: "anthropic/claude-sonnet-5", role: balanced),
        ],
        max_depth: 2,
    ),
)
```
