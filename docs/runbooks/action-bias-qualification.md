# Action-bias qualification

`cargo xtask action-bias` exports and recounts a fixed observation cohort without
contacting a model or opening the migrating session runtime. Export reads one
SQLite snapshot in read-only mode. Format 3 records the store ID, schema, UTC
window/cutoff, exporter identity, optional deployed producer pin, and every root
prompt shell or exec call. Schemas 45 through 50 are supported.

The start/end window is half-open. Cutoff is exactly end plus 24 hours; all three
boundaries are UTC midnights and cutoff must have elapsed. Persisted depth=0
identifies roots, including after a child loses its parent. Children, side
questions, runs outside the window and runs not finished before cutoff are
excluded. Every process-call state remains in the denominator.

Build one immutable exporter/recount executable from committed, clean source:

```sh
QQ_ACTION_BIAS_SOURCE_SHA="<full source commit>" cargo build -p xtask --release
cp target/release/xtask /tmp/qq-action-bias-classifier
sha256sum /tmp/qq-action-bias-classifier
```

The exporter identity covers the actual recognizer, execution argument parser,
receipt validator, image contracts, producer capture, descriptor encoding and
build policy digest. Both commands hash their running executable and record
platform/architecture. Recount requires the same exporter identity. Older export
formats must be regenerated; a recomputed footer cannot excuse another exporter.
Retain the clean source/build receipt. Linux hashes `/proc/self/exe`, so replacing
an executable pathname cannot replace the running image's identity.

Select the store reported by `qq config paths`. Outputs must be new files and
are published without overwriting artifacts. Exports contain raw argument,
receipt and plan JSON, which can include sensitive request text. Keep them local;
temporary artifacts use private permissions. Commit appropriate count/identity
receipts only.

```sh
/tmp/qq-action-bias-classifier action-bias export \
  --store "$HOME/.local/share/qq/sessions.sqlite3" \
  --start 2026-09-13 --end 2026-10-13 --cutoff 2026-10-14 \
  --output /tmp/qq-action-bias-baseline.jsonl
/tmp/qq-action-bias-classifier action-bias recount \
  --input /tmp/qq-action-bias-baseline.jsonl \
  --output /tmp/qq-action-bias-baseline-recount.json
```

Without a producer pin, context qualification remains unavailable. To verify a
new deployed cohort, establish the clean full source revision and executable
SHA256 from the accepted build/deployment receipt. Supply the same independent
pin to export and recount; do not derive it from the rows being evaluated:

```sh
/tmp/qq-action-bias-classifier action-bias export \
  --store "$HOME/.local/share/qq/sessions.sqlite3" \
  --start 2026-10-15 --end 2026-10-22 --cutoff 2026-10-23 \
  --producer-source "<accepted clean full source revision>" \
  --producer-sha256 "<accepted runtime executable SHA256>" \
  --output /tmp/qq-action-bias-candidate.jsonl
/tmp/qq-action-bias-classifier action-bias recount \
  --input /tmp/qq-action-bias-candidate.jsonl \
  --producer-source "<same accepted clean full source revision>" \
  --producer-sha256 "<same accepted runtime executable SHA256>" \
  --output /tmp/qq-action-bias-candidate-recount.json
```

V2 execution receipts bind the actual tool-call ID and canonical arguments to
captured process inputs. They retain the bounded canonical approved-image
manifest, its descriptor-bound digest, the compiled execution policy hash and
producer instance/source/running image identity. Export and recount use the same
core validator. VerifiedRead additionally requires image-family/hash membership,
held-directory identity, a supported locale, empty requested environment and
admitted argv. Recorded Work is an accounting verdict, not proof of mutation.
Producer identity is shared across plans and children; executable hashing occurs
once on bounded process-preparation workers. Cancelled or expired capture can
retry. Unknown or dirty source cannot satisfy a clean deployed producer pin.

Each line is bounded to 4 MiB and cohorts to one million calls. Before copying
SQLite values, export bounds arguments/descriptor JSON to 256 KiB each and
receipt/plan identity JSON to 16 KiB each. Oversized fields remain explicit
coverage gaps on retained calls. Recount rechecks these bounds, lexical command
consistency, identifiers/states, strict call ordering, count and header-inclusive
footer digest. Unknown fields, incomplete exports, duplicate/extra records,
unsupported schemas or altered identities fail qualification.

`lexical_read_share` measures validated shell command shape over shell calls.
Invalid timeout/cwd/env/command envelopes count as malformed, and explicitly
requested environments remain nonreads. Exec calls are counted separately.
`verified_read_calls` and `qualified_recorded_work_calls` require complete V2
proof and the independent producer pin. `execution_evidence_counts` explains
coverage gaps. `recorded_work_calls` retains valid recorded Work verdicts even
from V1, without promoting them to qualified evidence. The overall
`execution_context_qualified` flag requires coverage for every included call;
`verified_read_share` remains null otherwise. `execution_effects_qualified` stays
false: these receipts do not prove which mutations occurred.

Historical NULL/V1 receipts cannot recover inherited PATH/TMPDIR or producer
provenance and remain unqualified. No current manifest or build identity is
attached retrospectively. A missing receipt does not prove a process never ran.

A tooling receipt does not establish the seven-day treatment cohort or mark AB6
research accepted. Observation requires one pinned deployed runtime build,
retirement of earlier writers, seven complete UTC days and the research appendix's
other metrics, read identity and instruction coverage evidence. Mixed or
unverifiable producers leave the cohort unqualified. Paid evaluations require
ENG-809 authorization.
