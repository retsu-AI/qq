# Permissions and trust

QQ separates two questions:

- **Trust** — may this project's configuration influence QQ at all?
  Enabled explicitly for reviewed content with `qq trust`; task startup
  ignores sensitive project settings until then.
- **Approval** — may the agent take this action right now? Answered by the
  session's approval mode, the grants in effect, and, when neither decides,
  you.

## Project trust

QQ starts tasks using trusted configuration and non-sensitive project settings.
It does not ask you to approve a project before Auto can work. Untrusted project
providers, models, MCP servers, packs, and permission grants stay disabled;
QQ neither starts their commands nor records trust automatically.

Your global configuration, `QQ_CONFIG`, `QQ_CONFIG_CONTENT`, and managed
configuration remain trusted sources. Configure your model and connections
there when you want them available across projects.

A project can ship `.qq/config.ron`, `qq.ron`, `.qq/config.d/*.ron`, and
`.qq/packs/`. To enable its sensitive settings, review the content and explicitly
run `qq trust` in that project. Trust applies to the exact sensitive content;
changing it disables the changed settings until you explicitly trust them again.
Project pack entries must stay inside the project, including through symlinks.

```sh
qq trust
trusted /home/you/repo/.qq/config.ron
  declares: model, policy.allow_tools, policy.allow_shell_prefixes
```

`qq config sources` reports files awaiting trust. Explicit configuration inspection
commands still report pending trust so you can see which settings are withheld.
Task startup continues with the trusted settings instead of displaying a prompt.
Trust is stored in your QQ data directory, outside the project.

Sensitive settings include `model`, `worker_model`, `reviewer_model`,
`organization`, `providers`, `mcp`, `packs`, `profiles`, `delegation`,
`audit`, `approval_delegate`, `reasoning_effort`, and policy grants
(`allow_tools`, `allow_shell_prefixes`, `allow_hosts`, `shell_env`).
A project file that only sets `policy.exposed_tools` or `max_output_tokens`
loads without trust. Syntax, path boundaries, and managed policy still apply.

## Protected task files

Built-in `write_file` and `edit_file` refuse project configuration
(`.qq/config.ron`, `.qq/config.d/`, `.qq/packs/`, and `qq.ron`) and Git
trust files under `.git`, including nested repositories and symlink aliases.
Resolved configured pack directories inside the workspace (for example,
`vendor/reviewer`) and their aliases are protected too, even when that pack
is not selected for the current run. Unconfigured vendor directories remain
ordinary task files.
This applies in every approval mode; approving a file tool cannot grant it
permission to rewrite its own policy. Ordinary task files, `.github/`, and
`.qq/skills/` remain writable when the mode permits writes. A batch rejected
during planning changes no files; apply failures report any completed files.
Atomic writes pin the resolved parent directory and refuse a changed directory
namespace at apply-time validation. They do not follow replacement symlinks.
These checks do not lock out outside processes: another process can still
rename a directory after the final validation. They provide no OS isolation
for concurrent namespace changes, shell commands, or trusted MCP servers.

## Approval modes

Every session runs under one mode. Set the default in a profile
(`approval_mode`), change it in the TUI with `/approval`, or pass
`qq run --approval`.

| Mode | Reads | Edits and writes | Shell | MCP tools | `fetch` |
| --- | --- | --- | --- | --- | --- |
| `read_only` | run | denied | denied | denied | denied |
| `ask` | run | human approval each call | human approval each call | human approval each call | human approval each call |
| `auto` **(default)** | run | run within the workspace | run within the native task boundary | run | public hosts run |
| `full` | run | run | run without Auto containment | run | public hosts run |

Auto does not ask a human or consult a reviewer model for ordinary actions,
including actions taken by write sub-agents. Auto questions return a result
telling the agent to decide without human input. A denied operation returns a tool
error so the agent can choose another permitted approach. Denial does not grant
permission to bypass the boundary. Children remain subject to their parent's
permissions, including later revocation.

Ask requires fresh human approval for each non-read call. Stored grants and
`approval_delegate` cannot substitute for that approval. Headless Ask denies
immediately because no human is present. Approval in Ask applies once and does
not create a reusable grant.

### Native Auto execution

Shell and exec use the durable command worker. The worker retains command
identity, output and terminal status independently of the server connection.
Default Auto allows installed tools to read host files the user can already
read, and confines writes to the task workspace. Broad reads are not credential
privacy: readable host credential files remain readable.

Linux uses its native namespace and filesystem restrictions; macOS uses
Seatbelt. Windows uses a restricted caller token with workspace ACL grants.
The Windows baseline also permits writes to objects already writable by
Everyone or the logon SID, so it is weaker than Linux/macOS workspace-only
writes. Windows currently uses host networking; Linux/macOS request isolated
shell networking. `fetch` has a separate public-network policy.

Normal tool environment and HOME configuration are preserved. Credential-like
environment names are excluded unless explicitly requested through
`policy.shell_env`. Temporary files go into the writable task directory.
Default Auto adds no fixed per-command memory quota or cgroup setup requirement.
Embedders can explicitly request resource limits when their host supports them.

An embedding that does not supply a native task scope retains conservative
shell admission through read/build classification or explicit grants. This is
not equivalent to the binary's native Auto path.

### Command controls

The shell classifier is not a security boundary. It can describe an Ask
preview or admit conservative commands for an unscoped embedding; it does not
veto arbitrary programs inside native Auto, or restrict Full to a fixed set of
commands. Filesystem and network enforcement apply to scripts and libraries
regardless of the executable's name.

Configured command-prefix denies control literal commands; they cannot prevent
Python, a library, or a dynamically constructed command from performing the
same operation. Use native containment for those limits. Full still obeys
explicit managed policy and built-in protected-file checks.

## The approval prompt

```
◇ approval needed
$ npm install left-pad  (in ~/repo)
asks because: package_install
y once   a session   w workspace   n deny
```

| Key | Effect | Lifetime |
| --- | --- | --- |
| `y` | run this call | once |
| `a` | run this and every later call of the same shape in this session, when the grant fits | until the session ends |
| `w` | write the grant into `.qq/config.ron` and run, when the grant fits | every session in this workspace |
| `n` | deny; the model receives the denial as a tool error and continues | — |
| `Shift-Y` / `Shift-N` | decide and then steer the run with a note | — |
| `Esc` | deny, like `n`; after `Shift-Y` / `Shift-N` it first cancels the note and leaves the prompt open | — |

Ask approvals apply once. Session/workspace grant choices from older clients
are reduced to once under Ask. Stored grants remain useful for explicit
unscoped embeddings and retained policy configuration; native Auto does not
need a grant for every command.

Edits show a diff; `fetch` shows the URL and whether a grant covers the
host; MCP calls show the server, tool, and arguments.

When several sessions run at once, `Alt-A` / `Alt-D` approve or deny the
oldest waiting call in another session without leaving yours, and
`/attention` lists everything waiting.

A prompt waits for you. There is no server-side timer that denies it while
you are away: the hold ends when you answer, when the run's own deadline
(`--max-duration` or `RunLimits`) cancels the run, or when you cancel. If
you want a bound anyway — a shared server, an unattended supervisor — set
`approval_timeout_seconds` in configuration and the call is denied
`denied_timeout` after that many seconds, counted from when you were
actually asked. Auto never enters this human wait. Headless `qq run --approval ask` denies immediately
because no human is available.

## Grants in configuration

The `w` key appends to the `policy` section of the project's
`.qq/config.ron`:

```ron
policy: (
    allow_tools: ["edit_file"],
    allow_shell_prefixes: ["cargo test", "npm install"],
    allow_hosts: ["docs.rs"],
)
```

You can write the same by hand in any layer. Grants merge across layers;
`Remove("cargo test")` in a higher layer drops one a lower layer added; a
managed layer's `deny_shell_prefixes` removes it no matter who declared it.

Headless runs take grants on the command line:

```sh
qq run --approval auto --allow-shell "cargo test" --allow-tool write_file --allow-host crates.io "…"
```

## Tool catalog vs. approval

Approval decides whether a call *runs*. `policy.exposed_tools` and a pack
profile's `tools: (allow: …, deny: …)` decide whether the model *sees* a
tool at all. A tool removed from the catalog cannot be granted back with
`--allow-tool`; the model does not know it exists. Use the catalog for
"never", approval for "ask me".

## Network

`fetch` is refused for private, link-local, and cloud-metadata addresses
under every mode. `policy.allow_hosts` (exact names or `*.suffix`) lets
an unscoped caller record a host grant; native Auto can fetch public hosts
without a per-host grant. A managed `deny_hosts` wins.

## Where the rules live

The effect classes and execution details are in
[`../design/tools.md`](../design/tools.md). Historical ADRs record earlier
classifier and reviewer designs; the current behavior is described above.

Parent permissions cap child authority. An Ask parent requires fresh human
approval for each child action, while a read-only parent prevents mutation.
Changing the task or parent policy invalidates pending approval authority;
a late response cannot execute an action that is now denied.
