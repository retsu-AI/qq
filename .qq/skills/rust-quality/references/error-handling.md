# Typed errors, recovery, and retry policy

## 1. Decide policy before choosing syntax

For every fallible operation classify the outcome:

| Outcome | Caller action | Representation |
| --- | --- | --- |
| Legitimate absence | continue without a value | `Option<T>` |
| Invalid input/state | reject, preserving relevant state | specific error variant |
| Transient failure | bounded retry if safe | classified variant/source |
| Permanent failure | report/stop operation | specific actionable error |
| Cancellation | stop admitted work according to contract | distinct outcome where needed |
| Timeout/unknown write | reconcile before repeating effects | not automatically retryable |
| Proven internal invariant violation | fix bug; panic only with local justification | not an input-validation strategy |

Prefer exhaustive domain matches. For externally owned error enums that can grow,
follow their non-exhaustive contract with an explicit conservative policy; do not
invent unreachable cases. Keep recovery at the layer that has enough information.

## 2. Preserve context and sources

Complete synchronous example; call it only from an appropriate blocking context,
not directly on a Tokio worker. The read boundary is bounded to maximum+1 bytes,
so rejecting oversized input does not first allocate an unbounded String.

```rust
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
enum LimitFileError {
    #[error("cannot open limit file {path}")]
    Open { path: PathBuf, #[source] source: std::io::Error },
    #[error("cannot read limit file {path}")]
    Read { path: PathBuf, #[source] source: std::io::Error },
    #[error("limit file {path} exceeds {maximum} bytes")]
    TooLarge { path: PathBuf, maximum: usize },
    #[error("limit file {path} is not UTF-8")]
    Encoding { path: PathBuf, #[source] source: std::string::FromUtf8Error },
    #[error("limit file {path} does not contain an integer")]
    Parse { path: PathBuf, #[source] source: std::num::ParseIntError },
    #[error("limit in {path} must be between 1 and 64, got {actual}")]
    Range { path: PathBuf, actual: u32 },
}

fn read_limit(path: &Path) -> Result<u32, LimitFileError> {
    const MAXIMUM_BYTES: usize = 128;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(source) => return Err(LimitFileError::Open { path: path.into(), source }),
    };
    let mut bytes = Vec::new();
    file.take((MAXIMUM_BYTES + 1) as u64).read_to_end(&mut bytes)
        .map_err(|source| LimitFileError::Read { path: path.into(), source })?;
    if bytes.len() > MAXIMUM_BYTES {
        return Err(LimitFileError::TooLarge { path: path.into(), maximum: MAXIMUM_BYTES });
    }
    let text = String::from_utf8(bytes)
        .map_err(|source| LimitFileError::Encoding { path: path.into(), source })?;
    let actual = text.trim().parse::<u32>()
        .map_err(|source| LimitFileError::Parse { path: path.into(), source })?;
    if !(1..=64).contains(&actual) {
        return Err(LimitFileError::Range { path: path.into(), actual });
    }
    Ok(actual)
}
```

Here `?` is intentional propagation, and every conversion preserves the operation
and source. Path allocation happens on failure. The illustrative range must be
replaced by the real policy, not copied as QQ configuration. Filesystem containment,
symlink handling, and approval must still be enforced by existing workspace tools.

`#[from]` is useful when the source alone determines the domain variant. If the
same io::Error could mean open, read, sync, or rename failure, automatic conversion
cannot supply the operation/path; use explicit contextual mapping. Avoid blanket
transparent wrappers that expose low-level API details unnecessarily.

## 3. Recover only the expected case

**Recovery fragment**, using an existing `load` function/error enum:

```text
match load(path) {
    Ok(value) => use value,
    Err(NotFound { .. }) => use documented defaults,
    Err(Parse { .. } | Permission { .. } | Read { .. }) => report failure,
}
```

`load(path).unwrap_or_default()` would also mask corrupted files and permission
failures. `Result::ok` erases the reason altogether. Defaults are not a universal
error-handling policy. `Option::ok_or` is fine for a cheap missing-value variant;
use `ok_or_else` for allocated path/message context constructed only on absence.

Use `let Some(...) = ... else` for expected absence. Avoid `let Ok(...) = ... else`
when the error source or variant is needed for recovery/reporting.

## 4. Keep library contracts typed

Use existing domain errors/thiserror in reusable crates. Avoid `Result<T,String>`
and broad `Box<dyn Error>` where callers need classification. At the composition
root, an existing application report type may aggregate errors ergonomically;
do not let that erased interface leak back into protocol/runtime libraries.

Preserve the source chain rather than `map_err(|e| e.to_string())`. Error messages
are human-facing descriptions, not a stable control protocol. Tests should inspect
variants/fields/source kinds; assert message text separately only where it is a
user-facing contract. Never expose credentials or full prompt/response payloads
in Display, Debug, source logging, or tracing context.

## 5. Retry requires both classification and effect safety

Complete policy example, independent of the scheduling mechanism:

```rust
#[derive(Debug, Clone, Copy)]
enum Failure {
    RateLimited,
    Unavailable,
    InvalidInput,
    Cancelled,
    UnknownWriteOutcome,
}

#[derive(Debug, PartialEq, Eq)]
enum Recovery {
    Retry,
    Reconcile,
    Stop,
}

fn recovery(failure: Failure, attempts_used: u32, maximum_attempts: u32) -> Recovery {
    match failure {
        Failure::RateLimited | Failure::Unavailable => {
            if attempts_used < maximum_attempts { Recovery::Retry } else { Recovery::Stop }
        }
        Failure::UnknownWriteOutcome => Recovery::Reconcile,
        Failure::InvalidInput | Failure::Cancelled => Recovery::Stop,
    }
}

#[test]
fn exhausted_and_unknown_outcomes_are_not_blindly_retried() {
    assert_eq!(recovery(Failure::Unavailable, 3, 3), Recovery::Stop);
    assert_eq!(recovery(Failure::UnknownWriteOutcome, 1, 3), Recovery::Reconcile);
}
```

Actual orchestration must additionally:

1. Define attempts precisely, including the first attempt; reject zero/invalid
   configured limits rather than accidentally disabling bounds.
2. Cap attempts, per-attempt deadlines, cumulative time, and backoff. Validate
   arithmetic and externally supplied Retry-After values; cap excessive delays.
3. Check cancellation while waiting for admission, operation completion, and
   backoff. Supply deterministic jitter/time inputs for tests.
4. Preserve a stable idempotency key across retries where the protocol supports
   it. Never generate a new key for the same externally visible operation.
5. Reconcile unknown effects against authoritative storage/service state. A
   timeout, disconnected socket, or task cancellation does not prove rollback.
6. Retry at one responsible layer, not multiple nested loops multiplying load.

Only the provider layer owns provider-specific framing, transport, and retry
classification. Runtime code should act through the established neutral contract.
Do not recreate provider-specific status branches elsewhere.

## 6. Panics, cleanup, and observability

Replacing unwrap with expect improves a panic message, not failure behavior.
Check all panic sources: indexing, slicing UTF-8, integer overflow, RefCell borrow
conflicts, poisoned locks, task joins, todo/unimplemented/unreachable macros.
`debug_assert!` does not enforce an invariant in release builds.

Expected external failures return typed errors. A locally proven invariant may
justify an assertion/expect with a concise explanation, but prefer making the
invalid state unrepresentable. Tests can expect valid fixture setup. Do not use
catch_unwind as routine recovery; it does not cover aborting panics or undo effects.

A join result has two levels: task execution failure (`JoinError`) and operation
failure (`Result<T,E>`). Handle both; logging an operation error and returning
Ok creates silent partial success unless that is the explicit aggregate contract.

For best-effort cleanup, say why losing the cleanup is acceptable, preserve the
primary failure, and observe the secondary failure at the responsible boundary.
A Drop implementation cannot return a Result; required fallible cleanup needs an
explicit API before drop, with a safe drop fallback. Do not hide critical cleanup
behind `let _ =`, `.ok()`, or a detached asynchronous destructor.

Log once at the layer responsible for reporting/metrics. Preserve tracing context
without logging sensitive arguments; use `#[instrument(skip(...))]` or explicit
safe fields. Avoid logging the same error at every propagation hop.

## Test and review checklist

Cover each caller-visible error category, source preservation, legitimate absence
vs corruption, zero/max retry limits, exhaustion, cancellation during backoff,
unknown write reconciliation, duplicate operations, and cleanup failure. Check
failed operations never look durable/successful and errors contain no secrets.
