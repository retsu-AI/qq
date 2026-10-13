# Observability

Record enough context to diagnose a failure without collecting credentials,
private prompts, or raw customer records.

## QQ signals

QQ exposes health, capabilities, model catalog, session events, run outcomes,
tool approval state, and typed headless records. Its server and client code
bound response bodies, event cursors, deadlines, and stream idle time. Use
[Troubleshooting](troubleshooting.md) for the operator-facing reports.

The useful identity for a report is the exact source or binary revision, the
workspace or session scope, the command, the expected result, and the observed
result. Preserve uncertainty when a request may have reached a provider before
the client lost its reply.

## Privacy boundary

Diagnostics must redact provider secrets, bearer values, memory content,
prompt bodies, raw receipts, and private account data. Log bounded identifiers
and typed outcomes. Do not treat a hash as proof that an external service
accepted a request.

## Startup observability

The startup plan includes privacy-safe error tracking and deduplicated repair
routing. That composition is planned. It is not an enabled production
collector in this repository. A future implementation must name its capture
fields, retention, access boundary, and error-to-owner path before activation.

See [Recovery](startup-recovery.md) for the evidence needed after a failure.
