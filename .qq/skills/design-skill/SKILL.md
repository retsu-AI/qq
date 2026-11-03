---
name: design-skill
description: Design a new QQ skill or command: write SKILL.md so the model loads it at the right time, and validate it loads. Use when asked to create, extend, or fix a skill or slash command.
---

# Designing a skill or command

QQ has two customization units. Pick the one that fits.

| You want to... | Make a... |
| --- | --- |
| Teach the model a procedure it can pull in when relevant | **Skill** (`SKILL.md`) |
| Give the user a `/name` shortcut that runs a fixed instruction | **Command** (`name.md`) |

A model choice, approval mode or reasoning effort belongs in a `profiles`
entry in `config.ron`, not in a skill.

## Where things live

```text
.qq/skills/<name>/SKILL.md          workspace skills: disclosed to every profile
.qq/commands/<name>.md              workspace commands: disclosed, and /name leaves room for arguments
~/.config/qq/skills/<name>/SKILL.md user skills, every project
```

Skills under `.agents/skills/` are disclosed
too; a name in `.qq/` wins over the global root, which wins over
`.agents/`, so a skill copied into both `.qq/skills/` and `.agents/skills/` is
listed once. Skills under `.claude/` are read for `/name` invocation only. In
this repository every file under `.qq/skills/` has a byte-identical copy
under `.agents/skills/` for other harnesses; `cargo xtask lint` enforces it.

What QQ does not do, so write around it:

- `load_skill` returns `SKILL.md`, the skill's directory, and a list of up
  to 32 files beside it; it does not read them. Refer to bundled files by
  paths relative to the skill (`playbooks/x.md`) and have the model
  `read_file` them; QQ tells it they resolve against the skill directory.
- `$ARGUMENTS` in the body is replaced by the text after `/name`, or by the
  `arguments` the model passes to `load_skill`.
- Model-spawned sub-agents get no skills catalog and no `load_skill`. A
  skill that delegates must put the rules in the brief or tell the child
  to `read_file` the skill.
- A `/name` skill applies to one run. The next prompt does not see it
  unless the model loads it again.
- No front-matter flag hides a skill from the model; only its directory
  does.

## Writing a skill

The file is `<name>/SKILL.md`. The directory name is the skill name unless
front matter sets `name:`: 1–64
bytes, a lowercase letter, then lowercase letters, digits, `-`, `_`. A
directory with any other name is skipped without a warning. Reserved names are
every entry of `qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS` (`goal`, `btw`,
`ask`, `help`, `commands`, `models`, `profile`, `approval`,
`effort`, `skills`, `sessions`, `resume`, `agents`, `theme`, `editor`, `new`,
`clear`, `compact`, `rollback`, `prune`, `mouse`, `attention`, `changes`, `undo`,
`quit`, `exit`, and the `btw-` variants).

Start with YAML front matter carrying one interpreted key:

```markdown
---
name: <name>
description: One sentence, under 200 characters, that tells the model WHEN to load this.
---
```

QQ reads `description:`, as a single line capped at 512 bytes (a folded
`>` value reads as the literal `>`), and a top-level `name:`, which names
the skill when it is a valid, unreserved name. Keep `name:` equal to the
directory name; other harnesses need it. The
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

## Validate before you finish

1. Ask the server for the workspace capabilities (`POST /v1/capabilities`
   with the workspace id, or `/skills` in the TUI). The skill must be listed
   with its `source` and `disclosed: true`. If it is missing, check the
   directory name and the `name:` front matter against the rules above.
2. Start a session and check the system prompt evidence:
   `workspace_tools.skills.disclosed` counts your skill.
3. Load it once with `/name` and confirm the body and the skill directory
   reach the model.

## Anti-patterns

- A skill whose description is its name.
- A skill that restates tool safety rules; use `policy.exposed_tools` or an
  approval mode instead.
- A skill that carries the whole team's procedures; split by job.
- Editing `.claude/` expecting disclosure.
