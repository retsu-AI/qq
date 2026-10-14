# Action-bias qualification

`cargo xtask action-bias` exports and recounts a fixed observation cohort without
contacting a model or opening the migrating session runtime. The export reads
one SQLite snapshot in read-only mode. It records the store ID, schema, UTC
window and cutoff, every root prompt shell-call ID/run/state and command status,
and a footer count and digest. Schema 45 and 46 are supported.

The start/end window is half-open. Cutoff is exactly end plus 24 hours; all three
boundaries are UTC midnights and cutoff must have elapsed. Children, side
questions, runs starting outside the window and runs not finished before cutoff
are excluded. Every shell-call state in the cohort remains in the denominator.
Duplicate recognized fields or invalid typed shell envelopes (including cwd)
are checked by the runtime's own parser and count as malformed.
Malformed command input and commands provably rejected by the classifier's own
byte ceiling count as nonreads. An oversized argument envelope that prevents
inspection leaves the export unqualified rather than manufacturing a nonread.

Build one immutable classifier executable for both baseline and candidate.
The source must be committed and clean before building:

```sh
QQ_ACTION_BIAS_SOURCE_SHA="<full source commit>" cargo build -p xtask --release
cp target/release/xtask /tmp/qq-action-bias-classifier
sha256sum /tmp/qq-action-bias-classifier
```

The compiled source identity covers the actual progress-read recognizer and
its shared rules/bounds. Recount hashes the running executable and records its
platform and architecture. The build commit label alone is not evidence; retain
the clean source/build receipt and compare the executable and classifier-source
hashes in both recount receipts. Linux hashes `/proc/self/exe`, so replacing a
pathname cannot substitute a different image's hash for the running process.

Use the pinned executable for both exports and recounts. Explicitly select the
store path reported by `qq config paths`; the following path is the Linux
example. Outputs must be new files: the tool publishes them without overwriting
existing artifacts. Keep exports local because they contain command text;
commit only count/identity receipts appropriate for the project ledger.

```sh
/tmp/qq-action-bias-classifier action-bias export \
  --store "$HOME/.local/share/qq/sessions.sqlite3" \
  --start 2026-09-13 --end 2026-10-13 --cutoff 2026-10-14 \
  --output /tmp/qq-action-bias-baseline.jsonl
/tmp/qq-action-bias-classifier action-bias recount \
  --input /tmp/qq-action-bias-baseline.jsonl \
  --output /tmp/qq-action-bias-baseline-recount.json
```

Each line is bounded to 128 KiB, typed inspection envelopes to 256 KiB and cohorts to one
million calls. The export represents command strings only through the pinned
classifier's 16 KiB input ceiling, retaining larger strings as provably rejected
with their original byte count. Worst-case JSON escaping still fits the record
bound. Recount rejects unknown fields, invalid identifiers/states, duplicate or
out-of-order call IDs, missing/extra records, altered footer digests, mismatched
counts, unsupported schemas and zero denominators. Any such error leaves the
result unqualified.

The recount reports a read share; it does not prove a treatment cohort or mark
AB6 accepted. The candidate observation window requires AB1–AB5 on main,
installation of one pinned runtime build, retirement of prior writers and
verifiable build provenance for every included run. A merge date or prompt 20
alone cannot prove the treatment. Mixed or unverifiable runtime builds leave
the candidate window unqualified. Qualification also needs the research
appendix's other metrics, canonical persisted read identity and instruction
coverage evidence. Paid evaluations require ENG-809 authorization.
