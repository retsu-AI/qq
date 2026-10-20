# TUI Focus: sessions first, panes on demand, scoped selection

**Status:** Active from 2026-10-18. Parent
[ENG-1067](https://linear.app/retsu-ai/issue/ENG-1067). Ledger:
[`progress/tui-focus.md`](progress/tui-focus.md).
**Owner:** TUI lane.
**Owned paths:** `crates/qq-tui/**`, `docs/design/layout.md`,
`docs/design/transcript.md`, `docs/guide/tui.md`, `docs/runbooks/tui-qa.md`,
and the client bootstrap's session choice
(`crates/qq-client/src/interactive.rs`, `crates/qq-client/src/lib.rs`).

The [TUI redesign](tui-redesign.md) built the layout engine, tiers, rail and
inspector. This plan changes *what* those surfaces show. It does not replace
the open L4 and U8 slices there.

## How to read this plan

This is a UX plan. Its bullets describe the intended behavior for the
common cases. They are design goals that the PRs implement and test, not
invariants that every combination of terminal size, overlay, snapshot
timing and tree shape must satisfy. Only the items under **Guarantees**
are hard requirements. Everything else is best effort, and anything listed
under **Accepted limits** is out of scope for this plan; a review finding
that falls under one of those limits is not a defect in these PRs.

## Problem

Measured against the `sessions-*` and `tools-*` goldens on `2f585b0d`:

1. Every list is the whole session tree. One task that fans out to 20
   agents adds 20–40 rail rows and 20 `/sessions` rows.
2. The rail indents by tree depth inside state groups, so a child is drawn
   under an unrelated session.
3. `Auto` shows a 28-column rail for one conversation with one helper.
4. One child approval is shown three times in one frame.
5. Audit children are listed like user sessions.
6. The inspector's `Auto` takes 70–80 columns to say `Nothing expanded`.
7. There is no way to copy just the model's output: a drag does nothing and
   Shift-drag selects whole terminal rows across every pane.

## Vocabulary

- **Session**: a root conversation, or an agent whose parent is not loaded
  (shown as a session until the parent arrives).
- **Agent**: a sub-agent session under a loaded session.
- **Audit**: a `SessionPurpose::Audit` session and everything under it.

## Guarantees

These hold in every PR of the stack and each has a regression test:

1. **Audits are not user-facing.** No list, count, picker, navigation key,
   attention surface or fallback focus offers a session known to be under
   an audit, and none acts on one. "Known" means its audit ancestor is
   loaded; see the limits.
2. **Answers go to the call shown.** `Alt-A`/`Alt-D` answer the approval
   the composer rule names (or would name under a transient notice). They
   never answer an approval whose title is not on screen, and they never
   answer an `ask_user` hold yes or no (they focus it instead). A forced
   focus move drops a partly answered question or an armed amendment
   rather than sending it to another call.
3. **Copies are only text.** A copy never includes pane chrome from
   another pane (rail, inspector, borders) and never splits a grapheme
   cluster.
4. **No slower renders.** The render gate below.

## Slices

| ID | Issue | Goal |
| --- | --- | --- |
| P0 | [ENG-1068](https://linear.app/retsu-ai/issue/ENG-1068) | This plan and its ledger |
| F1–F2 | [ENG-1069](https://linear.app/retsu-ai/issue/ENG-1069) | Session roll-up and a sessions-only rail |
| F3 | [ENG-1070](https://linear.app/retsu-ai/issue/ENG-1070) | `/sessions` lists sessions; agents on expand or search |
| F4–F5 | [ENG-1071](https://linear.app/retsu-ai/issue/ENG-1071) | Less repetition; inspector only with content |
| F6 | [ENG-1072](https://linear.app/retsu-ai/issue/ENG-1072) | Clickable rail; selection scoped to the transcript, copied with OSC 52 |

### F1–F2: roll-up and rail

- The rail lists sessions in NEEDS YOU, WORKING and RECENT groups, one row
  each, with an `↳N` agent badge and the session's spend
  (`accounting.inclusive`, or the loaded tree's direct spend for older
  summaries; unknown if any part is unknown).
- Agents get rows only when they need the user (approval waiting on the
  human, or an unseen failure or finish), capped with `+N need you`, or
  when they belong to the focused session, capped with `+N more`.
- `Auto` shows the rail only when there are several sessions and room for
  it; otherwise a one-row strip counts sessions and needs.
- One attention order (approvals, then failures, then finishes; newest
  session first) feeds `Ctrl-G`, `Alt-A`/`Alt-D`, `/attention` and the rule.
- Startup: `qq --session ID` opens only a non-audit root; an attached or
  reconnecting TUI opens the newest non-audit root.

Acceptance: roll-up and rail tests for each need, the caps, the groups,
spend and audit exclusion; a `fanout` golden; startup tests.

### F3: picker

- `/sessions` lists sessions, newest first. `Tab`/`Right` expand a
  session's agents, `Left` collapses, a search also matches agents
  (`Session › Agent`), `Ctrl-A` lists every agent, `/agents` opens the
  focused tree.
- The cursor stays on its row when it can, and otherwise moves to that
  row's session.

Acceptance: app tests for each of those keys, the cursor fallback, and
audit exclusion.

### F4–F5: less repetition, panes on demand

- A need that the rail or a child's live card shows is not named again on
  the composer rule; the rule names the most urgent need nothing else
  shows and counts the rest. `related sessions` becomes one summary row.
- A transient notice keeps a short pointer to unshown needs on the right.
- The inspector's `Auto` shows only while it has content; the top row
  omits a known zero cost.

Acceptance: frame tests for the common layouts (Compact, Regular with and
without the rail, past the agent cap, under a notice); goldens moved
deliberately.

### F6: clickable rail and scoped selection

- A click on a rail row focuses that session.
- A drag in a transcript pane selects text in that pane only; release
  copies it over OSC 52 and says `copied N lines` (or that it was cut at
  the size bound). The copy drops the pane's own decoration (message rail,
  code gutter, list and quote prefixes) and rejoins soft-wrapped prose.
- `Esc`, keys, scrolling, resizing, or the selected text changing clears
  the selection. `/mouse` hands the mouse back to the terminal.
- Grapheme clusters are kept whole and measured with `unicode-width`'s
  `UnicodeWidthStr::width` of the whole cluster.

Acceptance: app tests for click-to-focus, drags in both directions, wide
and joined glyphs, wraps against newlines, the excluded prefixes and the
clearing rules; a terminal test for the OSC 52 bytes.

## Accepted limits

These are known and deliberately not addressed in this stack. Findings in
these areas are follow-ups, not blockers:

- **Exact single naming.** "Not named twice" is a goal for the layouts
  tested above. Other combinations (unusual overlays, workspace views,
  terminals near the minimum size, transient states between frames) may
  name a need twice or only point at it.
- **Redraw precision.** Which background events wake a frame, and when
  spinners animate, is tuned for the common cases. An extra redraw, or a
  stale spinner until the next event, is acceptable.
- **Live tree changes.** Sessions created, deleted or re-parented while a
  picker, form or workspace view is open are reconciled on a best-effort
  basis; the user may need to reopen it.
- **Bounded snapshots.** Orphans, cold bodies and the 512-session bound
  can leave counts, spend or ordering approximate until bodies load. A
  task agent whose audit ancestor is not loaded is shown until that
  ancestor arrives (closing that needs a protocol change, ENG-1080).
- **Terminal rendering.** Terminals and fonts that draw a cluster at a
  different width than `unicode-width`, ambiguous-width characters, and
  clusters outside the ZWJ, mark, flag, modifier and virama rules may
  misalign selection. Full UAX #29 needs a crate (a non-goal).
- **Copy fidelity.** Beyond the exclusions named in F6, a copy is the
  visible text; layout-only cells not listed there may appear in it.

## Non-goals

No new crates in `Cargo.lock`; F6 adds the workspace's existing `base64`
to `crates/qq-tui/Cargo.toml`. No protocol change, no persisted layout
preference, no split transcripts (L4), no word or line selection.

## Gates

- Workspace gates per `AGENTS.md`; `cargo xtask smoke` on each code slice.
- `cargo bench -p qq-tui --bench render` before and after each code slice:
  the `steady_state`, `streaming_focused`, `streaming_run_on_32kb`,
  `golden_path_first_minute`, `sessions_200_with_sidebar`,
  `children_20_with_sidebar` and `resize_horizontal` medians no more than
  5 % slower than the stack base (any speed-up passes).
- Each PR is done when its Codex review reports no major issues. Findings
  under **Accepted limits** are recorded as follow-ups rather than fixed in
  the PR.
