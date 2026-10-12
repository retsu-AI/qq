# TUI

## Sub-features

- **Slash commands** (`crates/qq-tui/src/commands.rs`, `docs/guide/tui.md`):
  - Help: `/help`, `/commands`.
  - Sessions and agents: `/sessions`, `/resume`, `/agents`, `/new`, `/clear`.
  - History: `/compact`, `/rollback`, `/prune`.
  - Settings: `/models`, `/profile`, `/approval`, `/effort`, `/delegate`, `/skills`, `/theme`, `/mouse`, `/attention`.
  - Side questions: `/btw`, `/ask`, `/btw-new`, `/btw-cancel`.
  - Other: `/changes`, `/editor`, `/quit`.
- **Keys:**

  | Key | Action |
  | --- | --- |
  | `F1` or `?` | help |
  | `Ctrl-K` | command palette |
  | `Ctrl-X` or `Esc Esc` | cancel |
  | `Alt-S` | interrupt |
  | `Ctrl-Q` | queue |
  | `Ctrl-O` | fold |
  | `Alt-R` | reasoning |
  | `Alt-N` / `Alt-C` | new session / new child |
  | `Ctrl-G` | next session that needs you |
  | `y` `a` `w` `n` | approvals |

## How to get to it (user POV)

Bare `qq`, or `qq --session ID`. Both stdin and stdout must be a terminal.

## Driving it headlessly

1. **Rendering:** `cargo nextest run -p qq-tui --test goldens` covers
   8 scenes × 5 sizes. Regenerate the frames with `QQ_UPDATE_GOLDENS=1`,
   then review `git diff crates/qq-tui/tests/goldens/`. For ANSI frames you
   can `cat`, run `cargo test -p qq-tui --test gallery -- --ignored`, which
   writes to `target/qq-tui-gallery/`.
2. **Live** (verified): use the isolated `--tui-qa-root` fixture from
   `docs/runbooks/tui-qa.md`, pointed at the fake from `qq-home.sh new`, and
   drive it through `script`, with no human:

   ```sh
   eval "$(.qq/skills/verify-qq/qq-home.sh new <name>)"
   ROOT=$(mktemp -d /tmp/qq-tui-qa.XXXXXX); chmod 700 $ROOT; mkdir $ROOT/config
   printf '(version: 1, model: "custom/qa-model", providers: {"custom": Custom(connection: (base_url: "http://127.0.0.1:%s/v1", api: OpenAiResponses, auth: NoAuth), models: {"qa-model": (name: "QA")})})' \
     $QQ_FAKE_PORT > $ROOT/config/config.ron
   (sleep 3; printf hello; sleep .5; printf '\r'; sleep 3; printf '\003') \
     | script -qfec "stty cols 100 rows 30; target/debug/qq --tui-qa-root $ROOT" /dev/null \
     > $QQ_HOME/evidence/tty.log
   sed 's/\x1b\[[0-9;?<>]*[a-zA-Z]//g' $QQ_HOME/evidence/tty.log | rg -o 'verify fake reply|qq --session [0-9a-f]+'
   case "$ROOT" in /tmp/qq-tui-qa.*) rm -rf -- "$ROOT";; esac
   ```

   The expected output is the fake's reply and the exit hint. tmux
   `send-keys` and `capture-pane` work the same way where tmux is installed.

## Gotchas

- The QA root refuses MCP, delegation, profiles, packs, remote providers,
  and stored credentials. Test those through `qq run` or `qq serve`.
- Each launch needs a fresh root.
- The TUI enables the kitty keyboard protocol, so `Alt-*` byte sequences
  depend on the terminal.
