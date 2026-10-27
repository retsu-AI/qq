---
name: design-skill-or-pack
description: Design a new QQ skill or agent pack: decide which you need, write the manifest and SKILL.md correctly, and validate it loads. Use when asked to create, extend, or fix a skill or pack.
---

# Designing A Skill Or Pack

QQ has two customization units. Pick the smaller one that fits.

| You want to... | Make a... |
| --- | --- |
| Teach the model a procedure it can pull in when relevant | **Skill** (`SKILL.md`) |
| Give the user a `/name` shortcut that runs a fixed instruction | **Command** (`name.md`) |
| Bundle a persona + skills + tool policy + model into a selectable profile | **Pack** (`pack.ron` directory) |

A skill is knowledge. A pack is an agent. Do not write a pack to hold one
skill; put the skill in `.qq/skills/` where every profile sees it.

## Where things live

```text
.qq/skills/<name>/SKILL.md          workspace skills: disclosed to every profile
.qq/commands/<name>.md              workspace commands: disclosed, and /name leaves room for arguments
.qq/packs/<id>/pack.ron             project pack (needs `qq trust`)
~/.config/qq/packs/<id>/pack.ron    user pack, every project
```

Skills under `.agents/` and `.claude/` are read for `/name` invocation only
and are never disclosed to the model. Put anything you want the model to find
on its own under `.qq/` or a pack. In this repository every file under
`.qq/skills/` has a byte-identical copy under `.agents/skills/` for other
harnesses; `cargo xtask lint` enforces it.

What QQ does not do, so write around it:

- `load_skill` returns only `SKILL.md`. Point at bundled files (playbooks,
  scripts) by their workspace path, such as
  `.qq/skills/<name>/playbooks/x.md`, and have the model `read_file` them.
- Model-spawned sub-agents get no skills catalog and no `load_skill`. A
  skill that delegates must put the rules in the brief or tell the child
  to `read_file` the skill.
- A `/name` skill applies to one run. The next prompt does not see it
  unless the model loads it again.
- No front-matter flag hides a skill from the model; only its directory
  does.

## Writing a skill

The file is `<name>/SKILL.md`. The directory name is the skill name: 1–64
bytes, a lowercase letter, then lowercase letters, digits, `-`, `_`. A
directory with any other name is skipped without a warning. Reserved names are
every entry of `qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS` (`goal`, `btw`,
`ask`, `help`, `commands`, `models`, `profile`, `approval`, `delegate`,
`effort`, `skills`, `sessions`, `resume`, `agents`, `theme`, `editor`, `new`,
`clear`, `compact`, `rollback`, `undo`, `prune`, `mouse`, `attention`, `changes`,
`quit`, `exit`, and the `btw-` variants).

Start with YAML front matter carrying one interpreted key:

```markdown
---
name: <name>
description: One sentence, under 200 characters, that tells the model WHEN to load this.
---
```

QQ reads only `description:`, as a single line capped at 512 bytes; a
folded `>` value reads as the literal `>`. `name:` is ignored by QQ but
other harnesses need it, so keep it equal to the directory name. The
catalog holds at most 64 skills and commands in total.

The description is the only part the model sees before it decides to call
`load_skill`. Write it as a trigger, not a title: "Run the verification
gates after a Rust change" beats "Verification". If the description does
not say when, the skill is never loaded. Nothing else in the front matter is
interpreted.

The body is plain Markdown, at most 64 KiB. Structure that works:

1. **When to use / when not to** — two or three lines.
2. **The procedure** — numbered steps with the exact commands or checks. The
   model will follow these literally; do not leave a step as "handle errors".
3. **Failure modes** — what goes wrong and what it looks like.
4. **What to report** — the shape of the answer you want back.

Rules that keep skills useful:

- One skill, one job. If you need an "and", split it.
- Prefer exact commands, file paths (`crate/src/file.rs:line`), and names
  over descriptions of them. The model can run a command; it cannot run a
  paragraph.
- Do not restate `AGENTS.md`. Reference it. Duplicated rules drift.
- A skill grants no authority. Loading one does not approve a tool or widen
  the workspace; if the procedure needs `shell`, the profile must allow it.
- Test the description by asking: given only this sentence and a user
  prompt, would the model know to load it? If not, rewrite the sentence.

## Writing a pack

A pack is a directory whose `pack.ron` declares one or more profiles. The
minimum that does something:

```ron
(
    schema: 1,
    id: "reviewer",          // 1–64 bytes: lowercase letter, then [a-z0-9-_]
    version: "0.1.0",
    name: "Code reviewer",   // optional display name
    requires: (protocol: 14),// optional; refuses to load on an older QQ
    profiles: {
        "reviewer": (
            model: "provider/model",   // optional; falls back to config
            approval_mode: read_only,  // read_only | ask | auto | full (lowercase)
            max_output_tokens: 8192,   // optional
            prompt: "prompts/persona.md",     // pack-relative, appended after AGENTS.md
            skills: ["skills"],               // dirs of <name>/SKILL.md
            commands: ["commands"],           // dirs of <name>.md
            tools: (
                deny: ["shell", "write_file", "edit_file"],  // deny wins
                // allow: ["read_file", "mcp__executor__*"]  // exact or prefix*
            ),
            mcp: ["executor"],   // subset of declared servers; absent = all
        ),
    },
    mcp: {
        // servers this pack brings with it, same shape as config.ron
    },
)
```

Bounds: 16 profiles per pack, 8 roots per list, 128 tool rules, 64 KiB
manifest, 64 KiB persona, 32 packs per load. Every profile name shares one
namespace with the `profiles:` map in `config.ron`; a duplicate is an error,
not an override. `default` cannot be declared.

Design choices worth making deliberately:

- **Tool policy is a filter, not a gate.** A denied tool is removed from the
  catalog before the model sees it. Use `deny` for "this agent must never",
  and leave the rest to approval mode. Prefer denying mutating tools over
  granting read-only ones; the built-ins are read-only by default.
- **Persona goes after `AGENTS.md`.** Write it as role, priorities, and
  output shape, not as rules the workspace file already states. Keep it
  under a screen; it costs tokens on every turn.
- **Skills in a pack are disclosed like `.qq/` skills** and show as
  `pack:<id>/skills/<name>/SKILL.md`. Put skills in the pack only when they
  are meaningless without the persona; otherwise put them in `.qq/skills/`.
- **`mcp:` subsets narrow, never widen.** A profile can only expose servers
  the pack or the configuration declared.
- **`requires.protocol`** should be the protocol version of the QQ you
  tested against (see `docs/design/protocol.md`).

## Validate before you finish

1. `qq trust` in the project if the pack is under `.qq/packs/` and this is the
   first sensitive change. Project packs are refused untrusted, silently
   from the model's point of view.
2. Ask the server for the workspace capabilities (`POST /v1/capabilities`
   with the workspace id, or the TUI's model/profile picker). The profile
   must appear with `pack: { id, version }`. If it does not, the manifest
   failed: the loader reports `InvalidPack`, `UnsupportedPackSchema`,
   `PackProfileConflict`, or `TooManyPacks` with the pack id.
3. Start a session on the profile and check the system prompt evidence:
   `workspace_tools.skills.disclosed` counts your skills, and a denied tool
   is absent from the catalog, not merely refused.
4. Every manifest error must be typed and name the pack; if you hit a
   panic or a generic string, that is a QQ bug worth filing, not a manifest
   problem to work around.

## Anti-patterns

- A skill whose description is its name.
- A pack with an empty profile (`Profile()` is not valid RON; give it at
  least one field).
- Restating tool safety rules in a persona instead of using `tools: (deny:)`.
- A pack that carries the whole team's skills; split by role.
- Editing `.agents/` or `.claude/` expecting disclosure.
