# Configuration reference

QQ reads [RON](https://github.com/ron-rs/ron) documents. Every file starts
with `(` and `version: 1,` and ends with `)`. Unknown keys are errors, so a
typo cannot silently do nothing. Files are limited to 1 MiB.

```ron
(
    version: 1,
    model: "anthropic/claude-sonnet-5",
    policy: (
        allow_shell_prefixes: ["cargo test", "cargo fmt"],
    ),
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
Sections `delegation` and `audit` replace as a whole.

| Order | Layer | Path |
| ---: | --- | --- |
| 1 | compiled defaults | built-in providers and policy |
| 2 | global packs | `<global>/packs/<id>/pack.ron` |
| 3 | organization manifest | cached from `qq org enroll` |
| 4 | **your global config** | `<global>/config.ron`, then `<global>/config.d/*.ron` sorted |
| 5 | project layers, repository root first, current directory last | per directory: `.qq/packs/<id>/pack.ron` (trusted only), `qq.ron`, `.qq/config.ron`, `.qq/config.d/*.ron` |
| 6 | explicit file | `QQ_CONFIG=/path/to/file.ron` |
| 7 | inline document | `QQ_CONFIG_CONTENT='(version: 1, …)'` |
| 8 | overrides | `--model` / `QQ_MODEL`, `--organization` / `QQ_ORGANIZATION`, `--max-output-tokens`, `QQ_JEV_CHECKPOINTS`, `QQ_JEV_ROUTING`, `QQ_JEV_APPROVAL`, `QQ_APPROVAL_DELEGATE` |
| 9 | managed | `/etc/qq/managed.ron` + `managed.d/` (Linux), `/Library/Application Support/qq/` (macOS), `%ProgramData%\qq\` (Windows); must be root-owned |
| 10 | MDM | macOS forced preference `dev.qq` / `ManagedConfig`; Windows policy `HKLM\Software\Policies\dev.qq\ManagedConfig` (`REG_SZ`); none on Linux |

`<global>` is `~/.config/qq` on Linux, `~/Library/Application
Support/dev.qq.qq` on macOS, `%APPDATA%\qq\qq\config` on Windows;
`qq config paths` prints it, along with the global `config.ron`, `tui.ron`,
and the data, managed, and organization paths, each marked `(exists)` or
`(missing)`. The global `config.ron` may be a symlink to a regular file;
project files may not.

Project layers that declare anything sensitive — `model`, `providers`,
`mcp`, `packs`, `profiles`, `delegation`, `audit`, Jev settings,
`reasoning_effort`, or any policy grant — are loaded only after `qq trust`
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
| `reviewer_model` | `"PROVIDER/MODEL"` | none | model used for `supervised` approval and the final-answer audit |
| `worker_model` | `"PROVIDER/MODEL"` | none | deprecated; use `delegation.roster` |
| `organization` | string | none | which enrolled organization manifest applies (`qq org`) |
| `max_output_tokens` | integer | `16384` | cap on generated tokens per model turn; a model's own limit applies if lower. One exception: when a turn is cut at this cap with nothing visible (all hidden reasoning) or right after a complete tool call, that one retry is sent with the cap doubled, up to the model's limit and never past a managed `policy.max_output_tokens`, at most once per run. Use `policy.max_output_tokens` for a hard ceiling; run budgets still bound spend |
| `reasoning_effort` | `none` `minimal` `low` `medium` `high` `xhigh` `max` `default` | provider default | effort hint for reasoning models that accept one. `default` lets the provider choose. Any other value outside the model's known accepted set (catalog or discovery) fails at plan time naming the accepted ones; with no known set, a rejected value fails at the provider ([providers](providers.md)) |
| `jev_review` | `off` `final` `enforce` | `off` | optional TypeSafe Jev checkpoints; see [`../runbooks/jev.md`](../runbooks/jev.md) |
| `jev_routing` | bool | `false` | optional Jev model routing |
| `jev_approval` | bool | `false` | Jev decides held approvals before `reviewer_model` and you; see [`../runbooks/jev.md`](../runbooks/jev.md#approval-delegate--jev_approval) |
| `approval_delegate` | `on` `off` | absent | who settles held approvals: `on` lets the delegate decide `ask` holds too, `off` sends every hold to you; absent keeps each mode's default (delegate under `auto`, you under `ask`). See [permissions](permissions.md#who-decides-a-held-call) |
| `approval_timeout_seconds` | `1`–`86400` | absent | server-side bound on how long a held call waits for you before it is denied. Absent is **no deadline**: the prompt waits for you, the run's own deadline, or cancellation. Not trust-gated: it can only shorten a wait |
| `providers` | map | built-ins | provider declarations; [below](#providers) |
| `mcp` | map | empty | MCP servers; [MCP servers](mcp.md) |
| `profiles` | map | empty | named per-session presets; [below](#profiles) |
| `packs` | map | discovered | agent packs by id; [below](#packs) |
| `delegation` | section | empty | sub-agent roster and bounds; [below](#delegation) |
| `audit` | section | `off` | final-answer audit; [below](#audit) |
| `policy` | section | see below | what runs without asking, and what may never run; [below](#policy) |

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

What the agent may do without asking, and what it may never do.

Any layer (your config, a trusted project):

```ron
policy: (
    // Grants: run without a prompt under `auto`.
    allow_tools: ["edit_file", "write_file"],
    allow_shell_prefixes: ["cargo test", "cargo fmt --all", "git status"],
    allow_hosts: ["docs.rs", "*.github.com"],
    shell_env: ["CARGO_HOME"],            // extra env vars shell may pass through
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
| `allow_tools` | any layer | tool names approved for the workspace. Grants layer: `"name"` adds, `Remove("name")` drops one a lower layer added |
| `allow_shell_prefixes` | any layer | shell commands approved by word-boundary prefix: `"cargo test"` covers `cargo test -p x`, never `cargo test \| sh` |
| `allow_hosts` | any layer | hosts `fetch` may reach under `auto`; exact or `*.suffix` |
| `shell_env` | any layer | variable names passed to shell children beyond `PATH HOME LANG TERM TMPDIR` |
| `builtin_preference` | any; only tightens | how hard the model is steered from shell habits to built-in tools |
| `exposed_tools` | any; intersects | the tool catalog; empty list exposes nothing |
| `allowed_providers` / `denied_providers` | managed layers only | which providers a model route may use; denies accumulate |
| `max_output_tokens` | managed layers only | ceiling for the top-level key; only lowers |
| `require_https` | managed layers only | reject `http://` custom endpoints (loopback exempt); default `true` |
| `allow_custom_providers` | managed layers only | allow `Custom`/`LiteLlm` declarations; default `true` |
| `allow_literal_secrets` | managed layers only | allow `Value(...)`; default `false` |
| `deny_tools`, `deny_shell_prefixes`, `deny_hosts` | managed layers only | remove grants no matter who declared them |

"Managed layers" are `managed.ron`, `managed.d/`, and MDM (rows 9 and 10
above). Setting one of these anywhere else fails with `managed-only policy
settings are only allowed in managed configuration`; to cap output tokens
for yourself, set the top-level `max_output_tokens` instead.

The approval prompt's `w` key appends to `allow_tools` /
`allow_shell_prefixes` / `allow_hosts` in the project's `.qq/config.ron`.
How grants interact with approval modes is in
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
(`read_only` `ask` `auto` `full`), `approval_delegate` (`on` `off`),
`jev_review`, `jev_routing`, `jev_approval`, `reasoning_effort`. Pack
profiles (below) add prompts, skills, and tool filters.

## `packs`

A pack is a directory with `pack.ron` that bundles profiles, a persona
prompt, skills, commands, and MCP declarations. Packs are discovered from
`<global>/packs/<id>/` and, once the project is trusted, `.qq/packs/<id>/`;
or declared explicitly:

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

Limits: 32 packs per load, 16 profiles per pack, 64 KiB manifest. A pack
profile shadows nothing: a profile of the same name in your config wins.

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

Up to 8 roster entries. Each route must resolve like `model`. An entry may pin
the reasoning effort its children run at with `effort: low` (any effort value);
without it, `fast` children run at `low` and `balanced` at `medium`, never above
the parent's own effort, fitted to what the route's catalog entry advertises
(a model without reasoning is sent none), while `strong` children inherit the
parent's. The deprecated `worker_model` counts as a `balanced` entry.
Design: [`../design/architecture.md`](../design/architecture.md) and
[`../plans/supervised-delegation.md`](../plans/supervised-delegation.md).

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
| `QQ_MODEL` | override `model` for this process |
| `QQ_ORGANIZATION` | override `organization` |
| `QQ_CONFIG` | an extra config file applied after project layers |
| `QQ_CONFIG_CONTENT` | an inline RON document applied after `QQ_CONFIG` |
| `QQ_JEV_CHECKPOINTS` | `off` `final` `enforce` |
| `QQ_JEV_ROUTING` | `on` `off` |
| `QQ_JEV_APPROVAL` | `on` `off`; overrides `jev_approval` for this process |
| `QQ_APPROVAL_DELEGATE` | `on` `off`; overrides `approval_delegate` for this process |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` or `GOOGLE_API_KEY`, `XAI_API_KEY` | built-in provider credentials when nothing is stored (`GEMINI_API_KEY` wins over `GOOGLE_API_KEY`) |
| `AWS_PROFILE`, `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`, `AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN`, `AWS_CONTAINER_CREDENTIALS_*` | Bedrock default credential chain |
| `TYPESAFE_API_KEY` | Jev, when not stored |
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
    reviewer_model: "openai/gpt-5.6",
    max_output_tokens: 16384,
    reasoning_effort: medium,
    policy: (
        allow_tools: ["edit_file", "write_file"],
        allow_shell_prefixes: ["cargo build", "cargo test", "cargo fmt --all", "git status", "git diff"],
        allow_hosts: ["docs.rs"],
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
