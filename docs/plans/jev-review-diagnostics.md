# JEV review decision diagnostics

## JD1: explain a final review without changing its decision

The original-session trial on signed `8591c70` produced two greetings, two
final reviews and one completion. The first review reported Contradicted with
aggregate confidence 0.23. The parser reports the minimum confidence across
three criteria, so this number cannot identify the confidence of the decisive
contradiction. Each criterion already requires confidence and winning
probability of at least 0.7. The retained event lacks those individual values.

JD1 adds the missing decision evidence. It does not claim to repair the greeting
or change the rubric, threshold, aggregation, repair policy or approval policy.
Its input is frozen `8591c705cf97ab4d38ea6dc0b4894e0af15aacd0`, rather than current
main, because the diagnostic must describe the installed reviewer contract.

The execution owner edits only `src/runtime.rs`, this plan, its progress ledger
and the relevant existing JEV design/runbook section. The integration owner
retains the candidate and supplies it to the existing runtime owner. Authentication,
live installation, provider calls and the original tmux session remain owned by
the recovery root. No remote publication belongs to JD1.

## Data and ownership

Use the existing checkpoint feedback/event path if the frozen source has no
suitable diagnostic sink. A validated response contributes exactly three
fixed-criterion diagnostic records containing the allowlisted choice, numeric
confidence, winning probability and resulting classified outcome. Preserve the
aggregate minimum confidence and outcome unchanged. Do not reflect unknown
response fields or partially validated responses. Invalid responses retain the
existing generic unavailable feedback.

Keep the diagnostic compact and statically bounded. No task text, evidence text,
raw provider payload, tool arguments, endpoints, credentials or model explanation
enters the new diagnostic. Do not add a file logger, dependency, CLI flag, public
protocol field or database migration. Reuse existing session event persistence
and correlation. Diagnostic text may become visible in the current review detail
surface; do not add a separate UI or a success claim.

No authorization or credential path changes. No new state, background task,
retry or I/O is introduced. Existing failure, cancellation, persistence and
replay ownership remain intact. Resource use is a fixed three-record allocation
bounded by existing feedback limits. Values must be represented accurately
enough to distinguish the 0.7 threshold without rounded values implying a pass.

## Preservation and recovery

The original accepted source, signed installation, signed rollback and protected
session backups remain untouched by this lane. A new isolated branch/worktree
holds JD1. Root controls any subsequent signed selection and trial, retaining
the prior binary and original session before changing runtime. Reverting JD1
removes diagnostic detail without changing stored schema or historical events.

## Acceptance

1. A mixed response with a high-confidence contradiction and a separate 0.23
   criterion still returns Contradicted with aggregate 0.23; the feedback shows
   which criterion caused each result.
2. Confidence below 0.7 and winning probability below 0.7 still classify that
   criterion as InsufficientEvidence. Supported, partial and high-confidence
   contradiction outcomes and spend remain unchanged.
3. Malformed contracts return the existing unavailable response and disclose no
   partial diagnostic. Extra attacker-controlled strings are never reflected.
4. A deterministic local reviewer fixture exercises response parsing through
   the existing review path and produces bounded evidence without live network
   services or real credentials. Existing final/advisory and strict tests pass.
5. Run focused tests while iterating, then repository format, Clippy, workspace
   build and workspace tests in the isolated credential-free environment. Obtain
   independent non-author review of the exact committed candidate.

The full user outcome remains one greeting, one final review and one completion
in the original session with history and rollback preserved. JD1 alone cannot
meet it. Root must read a changed bounded live trial before proposing a causal
behavior repair. Requested Computer testing also remains unverified because the
tool rejected Warp access for safety reasons; terminal/API traces are separate
evidence and must not be presented as visual testing.
