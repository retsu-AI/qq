# QQ TUI Themes

## Purpose

The TUI paints with a small set of semantic color roles. Themes map those roles
to concrete colors so the default look stays stable while users can ship their
own palettes without touching layout or keybinding config.

Themes are load-time configuration. Invalid themes fail before the TUI starts,
the same way invalid key chords do.

## Defaults

Two themes are always present and one of them is the default
([ADR 0036](../adr/0036-truecolor-default-theme.md)):

- `ink` is the designed default: a truecolor document
  (`crates/qq-config/themes/ink.ron`) with cool paper text, a sky accent, a
  copper brand, and a graphite surface. It is the default when the terminal
  advertises truecolor.
- `terminal` is the compiled ANSI fallback. It paints with the terminal's own
  named colors for every role but `brand` and `surface`, so it follows the
  user's terminal palette:

| Role | `terminal` |
|------|---------|
| `text` | white |
| `muted` | dark grey (dim) |
| `accent` | cyan |
| `brand` | `#ff9f43` |
| `warning` | yellow |
| `error` | red |
| `success` | green |
| `surface` | `#262830` (code-block background) |

Theme files use `#RRGGBB` literals only, so a named theme looks the same
everywhere; `terminal` is built in code because a file cannot name ANSI
colors.

These roles are the only colors the view should depend on. Attributes such as
bold, dim, and italic stay in the renderer; themes supply colors only.

## Selection

Theme selection lives in `tui.ron` beside bindings:

```ron
(
    version: 1,
    theme: "ink",
    bindings: (
        toggle_navigator: ["Ctrl-T"],
        // ...
    ),
)
```

`theme` is a string name. When omitted, or set to the alias `"qq"`, the
default rule picks the theme:

| `theme` | `COLORTERM` is `truecolor` / `24bit` | anything else or unset |
|---------|------|------|
| omitted or `"qq"` | `ink` | `terminal` |
| `"ink"` | `ink` | `ink` |
| `"terminal"` | `terminal` | `terminal` |
| any other name | that theme | that theme |

An explicit name always wins; only the omitted/`qq` case consults the
terminal. The `COLORTERM` comparison is case-insensitive and exact (no
trimming). The composition root reads the variable once at startup
(`src/main.rs`, `truecolor_support`) and passes a `TruecolorSupport` value to
`ConfigLoader::load_theme`; neither `qq-config` nor `qq-tui` reads the
environment. `qq` is an alias for the rule, not a palette: `default_theme_name`
rewrites it to `ink` or `terminal` before lookup, TUI settings still report
`qq` as the compiled default name, and no theme named `qq` is ever listed.

`tui.ron` continues to layer as today:

```text
compiled defaults
  → global tui.ron
  → project .qq/tui.ron from repository root to cwd
```

A global `tui.ron` or `themes/N.ron` may be a leaf symlink to a regular file.
Project `.qq/tui.ron` and `.qq/themes/N.ron` still reject symbolic links.

Later layers may override the theme name. The resolved name is then loaded once
when TUI settings are compiled. A user file named `ink.ron` shadows the shipped
`ink` (and the alias follows it); `terminal.ron` and `qq.ron` are ignored,
because those names never reach the file system.

## Theme Documents

Custom themes are RON documents named `<name>.ron`.

### Discovery

Given a selected name `N`, resolve the first match in this order:

1. Global `themes/N.ron` under the QQ configuration directory.
2. Project `.qq/themes/N.ron`, walking from the repository root to the current
   directory. The nearest file wins when several project layers define the same
   name.
3. Shipped themes compiled into the binary (below). A user file with the same
   name shadows the shipped copy, so any shipped theme can be copied out and
   tweaked.

Unknown names are a configuration error.

### Document Shape

```ron
(
    version: 1,
    // Optional color aliases referenced by role values.
    defs: {
        "base": "#191724",
        "surface": "#1f1d2e",
        "muted": "#6e6a86",
        "text": "#e0def4",
        "rose": "#eb6f92",
        "pine": "#31748f",
        "gold": "#f6c177",
        "foam": "#9ccfd8",
    },
    colors: (
        text: "text",
        muted: "muted",
        accent: "foam",
        brand: "rose",
        warning: "gold",
        error: "rose",
        success: "pine",
        surface: "surface",
    ),
)
```

Rules:

- `version` must be `1`.
- `defs` is optional. Keys are alias names; values are color literals.
- `colors` is required and must set every role listed below.
- Each role value is either a `defs` name or a color literal.
- Color literals are `#RRGGBB` (case-insensitive hex).
- Unknown fields are rejected.
- Missing roles, unknown alias names, and malformed literals are rejected.
- Alias cycles are rejected.

### Required Roles

| Role | Use |
|------|-----|
| `text` | Primary foreground |
| `muted` | Secondary / de-emphasized text |
| `accent` | Interactive highlights, user-turn markers, list markers |
| `brand` | Product mark (`qq` header) |
| `warning` | Pending work, caution, non-fatal attention |
| `error` | Failures, destructive emphasis, diff removals |
| `success` | Completed success, diff additions |
| `surface` | Recessed panel background (code blocks) |

Markdown and unified diffs reuse these roles in v1:

- headings and list markers → `accent`
- block quotes and chrome → `muted`
- inline code emphasis may use `warning` where the renderer already does
- diff additions → `success`
- diff removals → `error`
- diff hunk headers → muted `accent`

### Syntax Block

Highlighted code panels paint with eight syntax roles. Every one has a
default derived from the required roles, so a theme that says nothing about
syntax still colors code in its own voice; a theme may override any subset
with an optional `syntax` block:

```ron
(
    version: 1,
    defs: { /* ... */ },
    colors: ( /* the eight required roles */ ),
    syntax: (
        keyword:     "pine",
        function:    "rose",
        type:        "foam",
        string:      "gold",
        constant:    "gold",
        comment:     "subtle",
        property:    "foam",
        punctuation: "#908caa",
    ),
)
```

| Role | Highlights | Derived default |
|------|------------|-----------------|
| `keyword` | keywords, builtin variables (`self`, `this`) | `brand` |
| `function` | functions, methods, markup tags | `accent` |
| `type` | types, constructors | `warning` |
| `string` | string literals | `success` |
| `constant` | numbers, constants, escapes, attributes, labels | `brand` blended one third toward `text` |
| `comment` | comments (rendered italic) | `muted` |
| `property` | properties, object keys, operators, parameters | `text` |
| `punctuation` | brackets and delimiters | `muted` |

Rules:

- The block and every field inside it are optional. A missing field keeps
  the derived default; an empty block is the same as no block.
- Values follow the same rules as `colors`: a `defs` alias or a `#RRGGBB`
  literal, cycles rejected.
- Unknown fields inside `syntax` are rejected, like the rest of the document.
- No derived default is `error`, so code never reads as broken. The blend
  for `constant` mixes RGB channels; when `brand` or `text` is a terminal
  color (only the compiled `terminal` theme), `constant` is `brand` unchanged.
- The italic on comments is a renderer attribute, not a theme value.

Shipped themes declare a `syntax` block where the upstream palette
documents token colors (see § Shipped Themes); `ember` and `ink` rely on
the derived defaults.

## Runtime Model

`qq-tui` owns resolved theme values:

```rust
pub struct Theme {
    pub name: String,
    pub palette: Palette,
}

pub struct Palette {
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub brand: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
    pub surface: Color,
    // Derived: surface_alt, selection_bg, border, diff_add_bg, diff_del_bg,
    // info, and the eight syn_* roles (keyword, function, type, string,
    // constant, comment, property, punctuation).
}
```

`Theme::terminal()` returns the compiled ANSI palette and is what
`Theme::default()` builds when the root passes no themes; the designed default
`ink` is a theme document the root loads and passes in like any other.
`Theme::from_roles(name, [ThemeColor; 8])` builds a theme
from the root's resolved role colors without exposing a terminal library type
across the crate boundary; `Theme::from_roles_and_syntax(name, roles,
SyntaxOverrides)` additionally applies a document's `syntax` block after the
defaults are derived. `Palette::derive` is the single place the derived roles
come from, and `Palette::with_syntax` the single place overrides land.

Configuration loading:

1. Load layered `tui.ron` documents.
2. Resolve the effective theme name (the `qq` alias when unset).
3. Resolve the alias through the default rule (§ Selection) and load and
   validate the theme document for the resulting name.
4. Expand aliases into concrete colors, for `colors` and any `syntax` field.
5. Derive the syntax roles, then apply the document's `syntax` overrides.
6. Attach the resolved `Theme` to TUI options alongside layout and bindings.

The view must not hardcode palette colors. Style helpers (`normal()`,
`accent()`, `surface()`, and so on in `render.rs`) read the active `Palette`
from a thread-local the renderer refreshes at the top of every frame, so a
theme change costs one store per frame rather than a theme parameter on every
leaf renderer. Cached message layouts bake colors in, so the renderer compares
`App::theme_generation` each frame and drops every pane cache and the row diff
when it changes; the next frame repaints every row in the new palette.

## Shipped Themes

The compiled theme is `terminal`; its colors are the table in § Defaults. It
is built in code, never from a file, so it is always present and cannot be
shadowed.

The binary also ships these themes as ordinary theme documents
(`crates/qq-config/themes/*.ron`, embedded with `include_str!` and parsed by
the same code path as user files, so a broken shipped document is a build
defect that surfaces as a `ConfigError`):

| Name | Source | `syntax` block |
|------|--------|----------------|
| `ink` (default on truecolor) | QQ house theme: cool paper text, sky accent, copper brand | derived |
| `ember` | QQ warm theme: parchment text, teal accent, ember brand | derived |
| `catppuccin` | Catppuccin Mocha | style guide "Code Editors" table |
| `dracula` | Dracula | draculatheme.com/spec |
| `everforest` | Everforest dark medium | `colors/everforest.vim` tree-sitter links |
| `gruvbox` | Gruvbox dark medium | `colors/gruvbox.vim` (no punctuation tone) |
| `kanagawa` | Kanagawa wave | `themes.lua` `syn` table |
| `monokai` | Monokai Pro | monokai-pro.nvim syntax groups |
| `nord` | Nord | `nord.scss` per-color usage notes |
| `onedark` | One Dark | onedark.nvim highlights |
| `rose-pine` | Rosé Pine main | rose-pine/neovim highlight groups |
| `solarized` | Solarized dark | vim-colors-solarized (no property or punctuation tone) |
| `tokyonight` | Tokyo Night night | tokyonight.nvim base + tree-sitter groups |

Ports use the upstream palette's canonical hex values and map roles the way
the upstream theme uses those colors (its primary accent, its comment tone,
its elevated surface). Where a canonical tone fails contrast as text on the
theme's own background or surface, the port substitutes the palette's
nearest legible tone and says so in the file. Every shipped theme keeps all
eight roles distinct. Syntax fields follow the upstream editor theme's
token colors and are omitted (not invented) where the upstream declares
none, so the derived default applies. `qq config explain tui.theme` lists
the available names and where each comes from.

Users create custom themes by adding files under the global or project `themes`
directory and selecting the file stem from `tui.ron`:

```text
# global example
~/.config/qq/themes/rose-pine.ron

# project example
.qq/themes/rose-pine.ron
```

```ron
// tui.ron
(
    version: 1,
    theme: "rose-pine",
)
```

## Errors

Theme failures are configuration errors reported before the TUI starts:

- unsupported theme document version
- unknown theme name
- unreadable theme file
- parse failure
- missing required role
- unknown `defs` reference
- malformed color literal
- alias cycle
- a `syntax` field that fails to resolve (`ConfigError::InvalidThemeSyntax`
  names the field as a `SyntaxRole` and carries the `ThemeColorFault`:
  unresolved alias or literal, or alias cycle)
- an unknown field inside `syntax`

Provenance should record which source supplied the theme name and, when loaded
from disk, which path supplied the theme document.

## Theme Picker

`/theme` opens a picker listing every theme discoverable from the working
directory: `ink`, `terminal`, the other shipped themes, and user files. The
root passes the selected theme first, then the rest of the catalog; when the
selection came from the default rule, the first entry is the resolved `ink` or
`terminal`, so the `active` marker sits on the theme actually painting. `qq`
is an alias and never a row. Moving the cursor or typing a filter previews the highlighted theme
immediately; Enter keeps it for the rest of the session and shows the
`theme: "<name>"` line to add to `tui.ron`; Esc restores the theme that was
active when the picker opened. Each row paints a swatch of the theme's roles
in that theme's own colors. The picker does not write configuration: themes
remain load-time configuration, and the picker is a preview.

A theme file that fails to parse is skipped by discovery so one broken
experiment does not hide the picker; selecting it in `tui.ron` still fails
fast.

## Out Of Scope (v1)

- Per-role light/dark dual maps
- Terminal background clear / full chrome skinning beyond `surface`
- Hot reload of theme files while the TUI is running
- Writing the picker's choice back to `tui.ron`
- Importing foreign theme file formats
- Inline theme bodies embedded inside `tui.ron`

These can extend the same role model later without changing selection or
discovery.

## Implementation Sketch

1. Add `Theme` to `qq-tui` and thread it through `TuiOptions` / settings.
2. Replace hardcoded palette helpers in the view with theme-backed styles.
3. Extend `tui.ron` loading with an optional `theme` field.
4. Resolve theme files from compiled, global, and project locations.
5. Ship the compiled `terminal` theme and document the custom-theme workflow.
6. Tests: default resolution, layered name override, `defs` aliases, unknown
   name, incomplete theme, bad hex, and a render smoke path with a non-default
   palette.
7. Syntax roles on `Palette` with derived defaults, an optional `syntax`
   block per document, and shipped blocks where the upstream defines them.
8. `ink` as the truecolor default with `terminal` as the fallback and `qq` as
   the alias for the rule (ADR 0036).

## Design Constraints

- Prefer the smallest role set the renderer already needs.
- Keep theme files data-only; no scripting or conditional logic.
- Fail fast at load time rather than falling back silently to partial palettes.
- Do not leak configuration document types into the render hot path; pass one
  resolved `Theme`.
- Measure nothing exotic for v1: theme resolution runs once at startup.
