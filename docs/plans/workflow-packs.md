# Workflow packs: rigorous engineering modes for QQ

## Status

- Tracking: [ENG-1018](https://linear.app/retsu-ai/issue/ENG-1018), with
  sub-issues ENG-1019 to ENG-1027 (WP0 to WP8).
- Design only. Nothing has shipped.
- Ledger: [progress/workflow-packs.md](progress/workflow-packs.md).
- ADR-0057 (Proposed) covers skill supporting files and delegate skill pull.
- Prior art: Cursor's `pstack` plugin and how Codex, Claude Code, OpenCode,
  pi, fx, Cursor and Grok Build handle skills, agents and bundles (§ Survey).

## Goal

Selecting a profile such as `deep` makes QQ work the way a careful senior
engineer works on this repository:

- it matches the task to a playbook;
- it applies named principles;
- it delegates breadth and keeps judgment;
- it proves results against the real artifact;
- it replies plainly, with evidence.

Several small packs share one skill library. All of it lives in this
repository. It is our own development setup first, and other projects can copy
it.

## Survey

### pstack is one plugin

`pstack/.cursor-plugin/plugin.json` declares `skills/` and `agents/`. It has:

- about 50 skills;
- one router skill (`poteto-mode`, 20.5 KB, with 23 playbook files beside it);
- 24 principle skills hidden from the model;
- two agents.

It depends on a separate plugin, `cursor-team-kit`, for `deslop` and the
`control-*` skills. Only its README states that dependency. A third plugin,
`dyl-stack`, layers on top of it in the same way. The lesson for QQ is to split
by role and to make sharing structural, not a README note.

### How other harnesses handle this

| Concern | What the harnesses do | QQ today |
| --- | --- | --- |
| Skill format | All converge on Agent Skills `SKILL.md` (`name`, `description`) | Same; only `description` read |
| Hide from model | `disable-model-invocation` (Claude, Cursor, Grok, pi); Codex sidecar `allow_implicit_invocation` | Per root only |
| Supporting files | fx: `skill {location, resource}` confined to the skill directory. Codex, Claude, OpenCode, pi: absolute path plus general read | None (G1) |
| Children and skills | Claude, Codex, fx, OpenCode: children see the catalog | Children get none (G2) |
| Named agents | Claude, Cursor, OpenCode, Codex: Markdown or TOML files picked by `subagent_type`/`agent_type`. fx, pi core: none | Children inherit the parent profile only |
| Persona or mode | Claude output styles; Cursor "Use as Mode"; OpenCode primary agents; Codex `developer_instructions`; Grok personas | Pack persona on a profile |
| Bundles | Claude, Cursor, Codex: `plugin.json`, marketplaces, `plugin:skill` namespaces. pi: npm/git packages. OpenCode: code plugins. fx: none | `pack.ron`, `pack:<id>/` provenance |
| Catalog budget | Codex, fx: about 2 % of the window, entries dropped with a marker | 64 entries × 512 bytes |
| Persona or body size | Codex AGENTS.md 32 KiB total; Codex plugin skill body 8 KB; fx 64 KiB per file, 128 KiB total; Claude re-attaches 5 k tokens per skill | Persona 64 KiB hard cap |
| Hooks | Claude about 30 events; Codex 13; pi more than 40 in-process; fx 4 internal | Rejected (ADR-0003, ADR-0004) |

Takeaways:

1. **G1 is fx's design.** fx's `skill {location, resource}` tool, confined to
   the skill directory, is the closest match to QQ's confinement rule.
2. **G2 matches what other harnesses do.** Claude, Codex, fx and OpenCode give
   children the catalog. QQ is the outlier.
3. **Named agents are universal elsewhere.** QQ's equivalent is a profile. With
   several packs, the router needs to hand work to another pack's profile
   (WP8).
4. **Avoid the common failures** the survey found:
   - silent last-write-wins shadowing (OpenCode, pi);
   - keys that are accepted but ignored (Grok `model`, pi `allowed-tools`);
   - unbounded instruction injection (OpenCode, pi);
   - dependencies that only a README states (pstack);
   - code plugins (OpenCode, pi).

## Shape

### Packs and the shared library

A pack's skill roots are visible only while its profile is selected. A skill
shared by several packs therefore lives in `.qq/skills/`, which every profile
already sees. Packs carry only a persona and the skills that mean nothing
without it.

```text
.qq/skills/                        shared library, every profile
  principles/SKILL.md + <name>.md  one index entry, one file per principle
  plain-prose/                     replies, docs and PR bodies without AI tells
  test-first/                      failing test commit, then fix
  check-the-number/                vet a measurement before reporting it
  decision-log/                    TSV trail for long or unattended runs
  qq-verify/                       (exists) workspace gates
  design-skill-or-pack/            (exists) extended with supporting files

.qq/packs/deep/                    the router: rigorous general engineering
  prompts/persona.md
  skills/playbooks/ + <playbook>.md
  skills/{how,why,arena,swarm,interrogate,custom-playbook,deep-help}/
.qq/packs/reviewer/                (exists) read-only; interrogate's lens
.qq/packs/perf/                    (exists) gains hillclimb and trace playbooks
.qq/packs/protocol/                (exists) unchanged
```

Planned packs beyond these, added only when a playbook needs them:

- `autopilot`: unattended runs, babysit and shipping. Waits for `goals.md`
  G2–G3.
- `writer`: docs and ADRs. It may stay a `deep` playbook.

The index cap is fine. Shared skills plus the largest pack come to about 15
entries.

### Runtime changes (ADR-0057)

- **G1. `load_skill { name, file? }`.**
  - `file` is relative to the skill directory.
  - It is confined to the skill's opened root, so `..`, absolute paths and
    symlink escapes are refused.
  - The file follows the body contract: UTF-8, at most 64 KiB.
  - Its hash is recorded in prompt identity.
  - Commands accept no `file`.
- **G2. Split `RunCapabilities::allow_guidance`** into `select_guidance` and
  `pull_skills`.
  - Child task runs get `pull_skills` only.
  - Model-written task text never selects guidance.
- **G3. Hide flag: deferred.** G1 removes the need for one. Revisit only to
  disclose `.agents/skills` compatibility skills (which would read
  `disable-model-invocation`), or if the cap binds.

### Persona budget

Every request carries the persona, every child's included, because children
inherit the profile. It sits in the cached prefix, so the steady cost is close
to the provider's cache-read rate. A cache miss costs the full rate, once per
child.

| Reference | Size |
| --- | --- |
| pstack router | 20.5 KB |
| Codex AGENTS.md total | 32 KiB |
| Codex plugin skill body | 8 KB |
| Claude re-attach per skill | about 5 k tokens |

Rule for this plan:

- **Keep** the runtime hard cap of 64 KiB.
- **Authoring budget:** 8 KiB per persona, about 2 k tokens. A test over
  `.qq/packs/*/prompts/*.md` enforces it. The router lists triggers and names
  only; all detail moves to playbook and principle files under G1.
- **Measure** in WP4: prefix bytes, and cache-read tokens on turn 2 of a root
  run and a child run.

### `deep` persona contents

1. **Triggers to skills.** For example:
   - any code: `principles` `data-shape-first`;
   - before done: `qq-verify`;
   - replies: `plain-prose`;
   - numbers: `check-the-number`.
2. **Principles index.** One line per principle, naming when it applies.
3. **Playbooks.** Match the task, call `load_skill { name: "playbooks", file }`,
   and copy its steps into the todo list word for word.
4. **Autonomy.** Pause before irreversible actions: force-push, release tags,
   data deletion, external messages.
5. **Delegation.**
   - Roster roles: `fast` for breadth, `balanced` for code, `strong` for the
     hardest work and for judgment.
   - Panels use every distinct roster route.
   - Workers are read-only and return proposals. Write children are
     serialized and supervised.
   - The parent reviews every result.
6. **Reply shape.** Short sentences. Evidence or a label on every claim. No
   invented links.

### Principles kept

Each principle is tailored to `AGENTS.md`. Where a principle restates a rule
there, it cites the rule instead of copying it.

| Group | Principles |
| --- | --- |
| Core | `smallest-change`, `data-shape-first`, `subtract-before-add`, `attack-the-premise`, `minimize-reader-load`, `build-the-lever` |
| Architecture | `model-the-domain`, `boundary-discipline`, `types-encode-invariants`, `idempotent-operations`, `migrate-then-delete`, `separate-before-serializing` |
| Verification | `prove-it-works`, `fix-root-causes`, `verifiable-units`, `test-behavior` |
| Delegation | `guard-the-context`, `never-block-on-the-human` |
| Meta | `encode-lessons-in-structure` |

### `deep` playbooks

| Playbook | What it covers |
| --- | --- |
| `investigation` | Read-only question answered from evidence |
| `bug-fix` | Reproduce, find the root cause, fix |
| `feature` | New behavior |
| `refactor` | Structure change with behavior unchanged |
| `prototype` | Throwaway sketch to settle a question |
| `slice` | A `workflow.md` slice from start to PR |
| `pickup` | Take over another agent's work |
| `pause` | Leave work resumable |
| `opening-a-pr` | PR in this repository's format |

`perf` gains `hillclimb` and `trace-forensics`.

### `deep-setup` behavior

- Reads `qq config show` and writes `.qq/config.d/60-deep-local.ron`, which is
  git-ignored.
- `delegation` replaces the whole section, so the fragment carries forward
  `write_children`, `max_depth` and `default_role`.
- It overwrites the fragment on every run, so re-running gives the same
  result.

## Slices

| Slice | Goal | Inputs | Owned paths | Acceptance |
| --- | --- | --- | --- | --- |
| WP0 | Plan, ADR-0057, ledger, Linear issues | none | `docs/plans/workflow-packs.md`, ledger, `docs/adr/0057-*`, root reservation | Docs review |
| WP1 | `load_skill` supporting files (G1) | WP0 | `qq-core` `workspace/{skills,guidance,prepare}.rs`, `tools/specs.rs`, `runtime/prompt.rs`; `docs/design/tools.md` | Loads a file from a pack root and from a workspace root. Refuses `..`, absolute paths, symlink escapes, non-UTF-8, over 64 KiB, and a command with `file`. An unknown file is a tool error. Prompt identity records the file hash. Cancellation is honoured |
| WP2 | Delegate skill pull (G2) | WP0 | `qq-core` `RunCapabilities`, `sessions/execution.rs`, `plan.rs` prefix keys; `docs/design/tools.md` | A read child calls `load_skill`. A child task starting with `/name` is not selected. Summarizer and run keys stay equal (golden test). `context_assembly` bench before and after. Second reviewer |
| WP3 | Shared library in `.qq/skills/` | WP1 | `.qq/skills/{principles,plain-prose,test-first,check-the-number,decision-log}`, `design-skill-or-pack` | Capabilities list them. `load_skill principles prove-it-works.md` records a hash. Persona-budget test added |
| WP4 | `deep` pack: persona, playbooks, `how`, `why`, `arena`, `swarm`, `interrogate`, `custom-playbook`, `deep-help` | WP2, WP3 | `.qq/packs/deep/` | `qq config show` lists `deep@0.1.0`. Each workflow runs once on a real repository question within child bounds. Prefix bytes and turn-2 cache reads recorded |
| WP5 | `deep-setup`, `reflect`, `correct` | WP3 | `.qq/skills/` or a pack (Open decision 2) | Two runs of `deep-setup` produce byte-identical output that keeps `write_children` |
| WP6 | Retrofit `reviewer`, `perf`, `protocol` onto the shared library; `perf` playbooks | WP3 | `.qq/packs/{reviewer,perf,protocol}/` | Persona rules that duplicate the library are removed; budgets pass |
| WP7 | Guide page | WP4 | `docs/guide/` (GE1/GE3) | Shows `/profile deep` and the pack and library layout |
| WP8 | Proposed: `spawn_agent { profile }` so `deep` hands review to `reviewer` (ADR-0058, if accepted) | WP2 | `qq-core` `tools/specs.rs`, `sessions/subagents.rs` | Open decision 1 |

WP1 and WP2 are independent. WP3 can be drafted while WP1 is in review.

## Risks

- **Persona cost multiplies across children.** Mitigated by the 8 KiB budget
  and the WP4 measurement.
- **No evidence yet that playbooks improve outcomes.** That claim is a guess
  until `cargo xtask eval` runs paired arms (`token-efficiency.md` TE8).
- **Name collisions.** A `.qq/skills/<name>` that matches a pack skill name
  makes `/name` ambiguous. That is the intended loud failure, and
  `design-skill-or-pack` lists the names in use.

## Open decisions

1. **WP8.** Should a child be allowed to run on a different profile, chosen
   by name? Every surveyed harness with named agents allows this. The
   alternative is to keep inheritance only, so `interrogate` reviewers run on
   the `deep` persona with a review brief.
2. **Where setup and meta skills live.** Option A is `.qq/skills/`: always
   visible, three index entries. Option B is a separate pack: visible only
   under its own profile, which is awkward for a setup step. Proposed: A.
