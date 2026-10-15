# Pairwise JEV ranking

`qq jev rank` is an explicit, advisory comparison of two to four completed
candidate evidence records. It runs at most twelve counterbalanced TypeSafe
Choice requests and writes a user-only JSON receipt. It does not approve
tools, change routing, verify source, install a candidate, or alter QQ runs.

```sh
qq jev rank \
  --input candidates.json \
  --receipts comparison.json \
  --max-cost-usd 0.05 \
  --max-requests 12 \
  --deadline-seconds 60
```

The invocation deadline covers credential setup as well as both orientations.
By default `jev rank` resolves the protected system keyring credential, preserving
ordinary QQ credential precedence. For an explicitly authorized local QA run,
`--credential-source environment` uses the existing `TYPESAFE_API_KEY` process
environment value without writing or logging it; this opt-in flag does not change
general QQ authentication. A keyring setup that reaches the deadline is recorded
as an incomplete receipt instead of leaving the receipt in `running`. The
current synchronous OS-keyring API is not cancellable, so rank mode refuses its
default `keyring` source before lookup with `keyring_lookup_not_cancellable`;
ordinary QQ authentication keeps its existing keyring behavior. Use the
explicit environment source for a bounded local QA invocation until a
cancellable protected credential API exists.

The version-1 input must contain `task_id`, `criterion`, and two to four
`candidates`, each with `id`, `label`, `source_revision`, `artifact_digest`,
and explicitly selected non-secret `evidence`. Candidate evidence is bounded
to 16 KiB each and 256 KiB total. The receipt path must not already exist;
partial receipts are retained on interruption or budget exhaustion. A zero
budget creates a stopped receipt without a network request.

Both left/right orientations are requested for every pair. Conflicting
orientations become an evidence gap. Low confidence, insufficient evidence,
malformed responses, transport errors, and timeouts never produce a win.
Bradley–Terry strengths are reported only for a connected comparison graph;
they are preference estimates, not correctness probabilities. Existing
credentials are resolved through QQ's protected TypeSafe credential path.
Receipt usage records include a bounded input-token cost estimate using the
current pinned tariff of 42 nanodollars per input token; this is an estimate,
not a provider billing statement.

The command is separate from checkpoint review, approval delegation, and model
routing. It does not retry, resume, or promote a candidate. Pairwise results
should be fed only with compatible baseline/challenger artifacts from the
existing evaluation workflow, retaining failed trajectories as failures.
