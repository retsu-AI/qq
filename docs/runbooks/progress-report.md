# Progress report: are runs producing output?

Read-only queries over the sessions database. They measure whether long runs
and sub-agents produce output, rather than only activity. This is the AP0
baseline for [`autonomous-core.md`](../plans/autonomous-core.md) AP1–AP5
and the evidence behind ADR-0054.

Run it before and after each AP slice. Record the numbers in the plan's
ledger with the date, the store schema and the row counts. Never open the
live store read-write, and never paste session content into a ledger:
record counts only.

## Setup

The database is `sessions.sqlite3` in the data directory (`qq config paths`
prints it). The queries were written against store schema 39. `end` pins
the window, so a recorded baseline can be reproduced later. Every query
covers the 30 days before `end`.

```sh
db="file:$HOME/.local/share/qq/sessions.sqlite3?mode=ro"   # your data directory
end="2026-10-01"                                             # window end (UTC date); 30 days before it
q() { nix shell nixpkgs#sqlite -c sqlite3 -separator ' | ' "$db" "${1//@END@/$end}"; }
q "SELECT value FROM metadata WHERE key = 'schema_version';"   # the queries assume 39
```

`@END@` in a query is replaced with `end`. A run is in the window when
`started_at_ms` falls in [`end` − 30 days, `end`).

Definitions used below:

- A **long run** is a prompt run with at least 20 model turns, started in
  the window (30 days unless stated).
- A **child** is a run whose session has a `parent_id`.
- **Work** is a completed tool call whose effect is `mutating`, `shell`,
  `external` or `network`, excluding `spawn_agent`. It approximates
  ADR-0054's progress event. It differs from the event in three ways:
  - read-only shell commands and fetches count here, which makes stretches
    shorter;
  - failed non-read commands do not count here, which makes them longer;
  - spawn answers and steers are not counted.

  The no-work stretch is therefore an estimate of AP3b's report rate, not
  an exact replay.
- A **silent stretch** is the longest run of consecutive executed calls in
  one run with no work and no assistant text. It measures how long a run
  goes without saying or changing anything.
- A **no-work stretch** is the same, except that assistant text does not
  end it. That is ADR-0054's rule, where only work and requested reports
  count, so it predicts how often AP3b's report turn fires.

## 1. Silent and no-work stretches

Counts long runs whose longest stretch reaches 64, 128, 256 and 320 calls, split
by root and child and by whether the run completed. Set `text` to `1` for
silent stretches and `0` for no-work stretches.

```sh
stretches() { q "WITH rr AS (SELECT r.id, s.parent_id IS NOT NULL child, r.status FROM runs r JOIN sessions s ON s.id = r.session_id
     WHERE r.kind = 'prompt' AND r.started_at_ms >= strftime('%s','@END@','-30 days') * 1000 AND r.started_at_ms < strftime('%s','@END@') * 1000
       AND (SELECT count(*) FROM model_turns m WHERE m.run_id = r.id) >= 20),
ev AS (SELECT t.run_id, t.turn_ordinal o, t.call_ordinal c,
         (t.effect IN ('mutating','shell','external','network') AND t.state = 'completed' AND t.name <> 'spawn_agent') prog, 1 is_call
       FROM tool_calls t JOIN rr ON rr.id = t.run_id
       UNION ALL
       SELECT m.run_id, m.turn_ordinal, -1, 1, 0 FROM model_turns m JOIN rr ON rr.id = m.run_id
       WHERE $1 AND instr(m.assistant_content_json, '\"type\":\"text\"') > 0),
k AS (SELECT run_id, prog, sum(is_call) OVER (PARTITION BY run_id ORDER BY o, c ROWS UNBOUNDED PRECEDING) n FROM ev),
p AS (SELECT run_id, n FROM k WHERE prog UNION ALL SELECT id, 0 FROM rr UNION ALL SELECT run_id, max(n) + 1 FROM k GROUP BY run_id),
g AS (SELECT run_id, n - lag(n) OVER (PARTITION BY run_id ORDER BY n) gap FROM p),
m AS (SELECT rr.child, rr.status, max(g.gap) mg FROM g JOIN rr ON rr.id = g.run_id GROUP BY g.run_id)
SELECT CASE child WHEN 1 THEN 'child' ELSE 'root' END, status = 'completed', count(*),
       sum(mg >= 64), sum(mg >= 128), sum(mg >= 256), sum(mg >= 320) FROM m GROUP BY 1, 2;"; }
stretches 1   # silent
stretches 0   # no-work
```

Columns: kind, completed, runs, then how many reached 64, 128, 256 and 320
calls. A turn executes at most 16 calls, so a 320-call stretch needs at
least 20 turns. The long-run filter therefore misses no run that could have
one.

## 2. When children first say anything

For long children: how many runs, the median turn of the first text, and
how many runs had no text before their final turn.

```sh
q "WITH ch AS (SELECT r.id,
     (SELECT max(turn_ordinal) FROM model_turns m WHERE m.run_id = r.id) last,
     (SELECT min(turn_ordinal) FROM model_turns m WHERE m.run_id = r.id AND instr(m.assistant_content_json, '\"type\":\"text\"') > 0) ft
   FROM runs r JOIN sessions s ON s.id = r.session_id
   WHERE s.parent_id IS NOT NULL AND r.started_at_ms >= strftime('%s','@END@','-30 days') * 1000 AND r.started_at_ms < strftime('%s','@END@') * 1000
     AND (SELECT count(*) FROM model_turns m WHERE m.run_id = r.id) >= 20),
o AS (SELECT coalesce(ft, 99999) f FROM ch ORDER BY f), n AS (SELECT count(*) c FROM o)
SELECT (SELECT c FROM n), (SELECT f FROM o LIMIT 1 OFFSET (SELECT c / 2 FROM n)),
       (SELECT count(*) FROM ch WHERE ft IS NULL OR ft >= last);"
```

A median of `99999` means most long children never produced text.

The same count, restricted to children that executed 64 or more calls. This
is the population AP3b's report turn applies to, and Goal 6's second number:

```sh
q "WITH ch AS (SELECT r.id,
     (SELECT max(turn_ordinal) FROM model_turns m WHERE m.run_id = r.id) last,
     (SELECT min(turn_ordinal) FROM model_turns m WHERE m.run_id = r.id AND instr(m.assistant_content_json, '\"type\":\"text\"') > 0) ft
   FROM runs r JOIN sessions s ON s.id = r.session_id
   WHERE s.parent_id IS NOT NULL AND r.started_at_ms >= strftime('%s','@END@','-30 days') * 1000 AND r.started_at_ms < strftime('%s','@END@') * 1000
     AND (SELECT count(*) FROM tool_calls t WHERE t.run_id = r.id AND t.state IN ('completed','failed')) >= 64)
SELECT count(*), sum(ft IS NULL OR ft >= last) FROM ch;"
```

## 3. Slice checkpoints answered with tool calls

The slice checkpoint asks for a tool-free progress record. The first query
counts checkpoint turns the model answered with tool calls, and how many of
those also had text. The second counts runs that reached a checkpoint, and
how many of them skipped one.

```sh
q "WITH ck AS (SELECT DISTINCT run_id, turn_ordinal FROM tool_calls
     WHERE result LIKE 'not executed: this reply was the slice checkpoint%')
SELECT count(*), sum(instr(m.assistant_content_json, '\"type\":\"text\"') > 0), count(DISTINCT ck.run_id)
FROM ck JOIN model_turns m ON m.run_id = ck.run_id AND m.turn_ordinal = ck.turn_ordinal;"

q "WITH c AS (SELECT run_id, count(*) n FROM tool_calls WHERE state IN ('completed','failed') GROUP BY run_id HAVING n >= 241)
SELECT count(*), sum(EXISTS (SELECT 1 FROM tool_calls t WHERE t.run_id = c.run_id
  AND t.result LIKE 'not executed: this reply was the slice checkpoint%')) FROM c;"
```

These queries cover the whole store, not a window. They key on the
rejection text RR1 introduced, so they only count checkpoints since RR1
(#108) shipped, and they cannot see a checkpoint the model answered with
text only or with nothing. AP3a adds the turn's kind to `model_turns`;
that PR replaces this section with a query on the kind.

## 4. Time parents spend blocked on children

For prompt runs that called `spawn_agent`: how many runs, their total wall
minutes, and the minutes during which at least one `spawn_agent` call was
open. Overlapping calls are merged, so parallel children count once.

```sh
q "WITH r AS (SELECT r.id, r.started_at_ms s, coalesce(r.finished_at_ms, r.started_at_ms) f FROM runs r
     WHERE r.kind = 'prompt' AND r.started_at_ms >= strftime('%s','@END@','-30 days') * 1000 AND r.started_at_ms < strftime('%s','@END@') * 1000
       AND EXISTS (SELECT 1 FROM tool_calls t WHERE t.run_id = r.id AND t.name = 'spawn_agent')),
c AS (SELECT t.run_id, t.requested_at_ms a, coalesce(t.finished_at_ms, t.requested_at_ms) b FROM tool_calls t JOIN r ON r.id = t.run_id
      WHERE t.name = 'spawn_agent'),
o AS (SELECT run_id, a, b, max(b) OVER (PARTITION BY run_id ORDER BY a, b ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) prev FROM c),
g AS (SELECT run_id, a, b, sum(CASE WHEN prev IS NULL OR a > prev THEN 1 ELSE 0 END) OVER (PARTITION BY run_id ORDER BY a, b) grp FROM o),
m AS (SELECT run_id, min(a) a, max(b) b FROM g GROUP BY run_id, grp)
SELECT (SELECT count(*) FROM r), (SELECT sum(f - s) / 60000 FROM r), (SELECT sum(b - a) / 60000 FROM m);"
```

After AP4, unbounded read spawns return on admission. Add `wait_agents`
calls to `c`, and the parent's wait for children (recorded by AP4) to
the same union, so the figure keeps measuring time spent waiting.

## 5. Whether children do any work

For long children: how many runs, how many made a mutating call, and how
many ran a shell command.

```sh
q "SELECT count(*),
       sum(EXISTS (SELECT 1 FROM tool_calls t WHERE t.run_id = r.id AND t.effect = 'mutating' AND t.state = 'completed')),
       sum(EXISTS (SELECT 1 FROM tool_calls t WHERE t.run_id = r.id AND t.effect = 'shell' AND t.state = 'completed'))
FROM runs r JOIN sessions s ON s.id = r.session_id
WHERE s.parent_id IS NOT NULL AND r.started_at_ms >= strftime('%s','@END@','-30 days') * 1000 AND r.started_at_ms < strftime('%s','@END@') * 1000
  AND (SELECT count(*) FROM model_turns m WHERE m.run_id = r.id) >= 20;"
```

Read children are read-only by construction, so both are expected to be 0.
Their only output is text, which is why sections 1 and 2 matter.

## 6. What `spawn_agent` returned

For `spawn_agent` calls in the window: the total, how many returned an
error rather than an answer, and how many of those errors were children
that exceeded their context.

```sh
q "SELECT count(*), sum(is_error),
       sum(is_error AND (result LIKE 'the sub-agent run failed: context is estimated%'
                      OR result LIKE 'the sub-agent run failed: session context reached%'))
FROM tool_calls WHERE name = 'spawn_agent'
  AND requested_at_ms >= strftime('%s','@END@','-30 days') * 1000 AND requested_at_ms < strftime('%s','@END@') * 1000;"
```

## Recording

Record the following in the ledger:
- the window end, the store schema (from Setup), and `SELECT count(*) FROM runs`;
- each section's row;
- which AP slices were on `main` during the window.

A measurement window must be at least 7 days of normal use, and it must
start after the slice being measured merged.
