# ADR-0036 — Designed truecolor default theme `ink` with `terminal` ANSI fallback

**Status:** Accepted
**Date:** 2026-09-21
**Deciders:** tui-redesign plan D1
**Implements:** [`theme.md` § Selection](../design/theme.md#selection)

## Context

Until this decision the TUI's default palette was the terminal's own ANSI
colors (`text` white, `muted` dark grey, `accent` cyan, and so on) plus two
fixed RGB values for `brand` and `surface`. That palette varies with every
terminal emulator and user color scheme: "dark grey" is unreadable on some
and indistinguishable from "white" on others, and the surface tint the
redesign depends on (code panels, inline code, selection rows, diff tints in
`docs/plans/tui-redesign.md` § S3–S4) cannot be expressed in ANSI colors at
all. The redesign's contrast and rhythm work was being reviewed against a
default nobody could reproduce.

`ink` (`crates/qq-config/themes/ink.ron`) is the house theme designed for
that redesign. Shipping it as the default needs a rule for terminals that
cannot show 24-bit color, and a story for the `theme: "qq"` already written
in user configuration, which named the ANSI palette.

## Decision

- The compiled ANSI palette is named `terminal`. It is selectable by name
  everywhere a theme is and remains what `qq-tui::Theme::default()` builds
  when the composition root passes no themes.
- When the user has not set a theme, the default is `ink` if the terminal
  advertises truecolor and `terminal` otherwise. Truecolor is advertised
  when `COLORTERM` is `truecolor` or `24bit`, compared case-insensitively;
  any other value or an unset variable is "not advertised".
- Detection happens once, in the composition root (`src/main.rs`,
  `truecolor_support`), and is passed into `qq-config` as a
  `TruecolorSupport` value. Neither `qq-config` nor `qq-tui` reads the
  environment.
- `qq` stays a valid theme name as an alias for that rule:
  `ConfigLoader::load_theme(cwd, "qq", truecolor)` resolves to `ink` or
  `terminal` before lookup. TUI settings still carry `qq` as the compiled
  default name, so provenance for an unset theme is unchanged. There is no
  third palette named `qq`.
- Any explicit name is taken literally: `theme: "terminal"` on a truecolor
  terminal paints ANSI colors, `theme: "ink"` on a terminal that does not
  advertise truecolor paints `ink`. Only the unset/`qq` case consults the
  detection result.

## Consequences

- Positive: a fresh install on a modern terminal gets the designed palette,
  so the redesign's surfaces, tints, and contrast read as intended. Users on
  terminals without a truecolor advertisement keep exactly the look they had.
  Existing `theme: "qq"` configurations keep working and now follow the
  default rule, which is the same or better than before.
- Negative / risks: `COLORTERM` is a convention, not a capability query. A
  terminal that supports truecolor but does not set it (some multiplexers
  strip it) falls back to `terminal`; the fix is `theme: "ink"`. A terminal
  that sets it falsely paints `ink` in approximated colors; `theme:
  "terminal"` opts out. `qq config check` and the TUI read the variable
  from their own process, so they agree only when run in the same
  environment.
- The theme picker lists `ink` and `terminal` (with every other theme) and
  never `qq`; the active marker follows the resolved document's name, so the
  default pick is marked whichever way the rule went. The single-theme notice
  now names `terminal`.
- `qq-config` exports `TERMINAL_THEME`, `TRUECOLOR_DEFAULT_THEME`,
  `TruecolorSupport`, and `default_theme_name`; `load_theme` gains the
  `TruecolorSupport` parameter. `Palette::QQ` / `Theme::qq()` in `qq-tui`
  are `Palette::TERMINAL` / `Theme::terminal()`.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Keep the ANSI palette as the default | It is a different palette on every terminal; the redesign's surface and tint roles cannot be carried by ANSI colors, so the default would never show the designed result |
| Detect truecolor from terminfo (`Tc`/`RGB` capabilities) | Requires a terminfo reader dependency and a database that is frequently stale for the terminals that matter; `COLORTERM` is what those terminals actually set |
| Probe with an OSC/DCS query at startup | Adds a round-trip before the first frame, needs a timeout, and misbehaves through multiplexers and SSH; startup latency is a product requirement |
| Make `qq` a separate compiled palette | Three palettes to maintain and a name in every existing config that means "the old look", freezing the default forever |
| Always default to `ink` | Non-truecolor terminals would approximate eight RGB roles to 16 or 256 colors, losing the contrast the roles were chosen for |

## Evidence / references

- `src/main.rs` (`truecolor_support`, `load_tui_themes`),
  `crates/qq-config/src/theme.rs` (`default_theme_name`, `load`),
  `crates/qq-tui/src/theme.rs` (`Palette::TERMINAL`, `Theme::terminal`)
- Tests: `colorterm_detection_reads_truecolor_and_24bit_case_insensitively`,
  `the_default_theme_rule_picks_ink_on_truecolor_and_terminal_otherwise`,
  `the_qq_alias_resolves_to_ink_or_terminal_by_truecolor_and_explicit_names_win`,
  `the_theme_picker_lists_ink_and_terminal_and_marks_the_default_rule_pick_active`
- `docs/plans/tui-redesign.md` § S6, decision D1; ledger
  `docs/plans/progress/tui-redesign.md` U5 receipt
