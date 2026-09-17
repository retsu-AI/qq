const STORAGE_CONTEXT_BYTES: u64 = 4 * 1024 * 1024;
const CONSERVATIVE_OUTPUT_BYTES_PER_TOKEN: u64 = 32;
/// Bytes of provider-neutral request text per estimated input token when no
/// provider measurement covers the request. English prose and source code
/// tokenize at roughly 3.5–4.5 bytes per token on every current tokenizer;
/// four is the figure Codex, pi, fx, and OpenCode all use. Rounding up and
/// the output reserve keep the estimate conservative; a provider-reported
/// overflow remains the authoritative backstop.
pub(crate) const ESTIMATED_BYTES_PER_TOKEN: u64 = 4;

/// Estimated input tokens for `bytes` of request text, rounded up.
pub(crate) const fn estimate_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(ESTIMATED_BYTES_PER_TOKEN)
}

/// Fraction of the model window held back as headroom before a prompt run
/// starts: an eligible run whose estimate exceeds `window - reserve` compacts
/// proactively, while it still fits, instead of waiting for the estimate to
/// cross the window itself. Codex compacts at 90 %; fx at 80 %.
const PROACTIVE_COMPACTION_HEADROOM_DIVISOR: u64 = 10;
pub(crate) const COMPACTION_INSTRUCTION_BYTES: usize = 64 * 1024;
const COMPACTION_STORAGE_ENVELOPE_BYTES: u64 = COMPACTION_INSTRUCTION_BYTES as u64 + 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextConstraint {
    ModelWindow,
    StorageBackstop,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextTarget {
    pub(crate) max_reducible_input_tokens: Option<u64>,
    pub(crate) max_reducible_input_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextRejectReason {
    Irreducible(ContextConstraint),
    NoReducibleHistory(ContextConstraint),
    AlreadyAttempted(ContextConstraint),
    BetweenRunsOnly(ContextConstraint),
    Unsupported(ContextConstraint),
    ProviderReportedOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionDisposition {
    Eligible,
    AlreadyAttempted,
    BetweenRunsOnly,
    Unsupported,
    /// The request *is* the summarizer's: its transcript is the one being
    /// shrunk, so an estimated model-window overflow is expected rather than
    /// disqualifying. Only the storage backstop applies; a provider-reported
    /// overflow of the summarizer request is the authoritative failure.
    Summarizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextEstimate {
    pub(crate) input_bytes: u64,
    pub(crate) estimated_input_tokens: u64,
    pub(crate) output_reserve_tokens: u64,
    pub(crate) storage_reserve_bytes: u64,
    pub(crate) context_window: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextPlan {
    Send {
        estimate: ContextEstimate,
    },
    Compact {
        estimate: ContextEstimate,
        reason: ContextConstraint,
        target: ContextTarget,
    },
    Reject {
        estimate: ContextEstimate,
        reason: ContextRejectReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextInput {
    pub(crate) context_window: Option<u32>,
    pub(crate) max_output_tokens: u32,
    pub(crate) system_bytes: u64,
    pub(crate) tool_schema_bytes: u64,
    pub(crate) reducible_message_bytes: u64,
    pub(crate) irreducible_message_bytes: u64,
    /// Provider-measured occupancy of a caller-verified compatible prefix,
    /// including a conservative byte upper bound for newly appended input.
    pub(crate) compatible_input_tokens: Option<u64>,
    pub(crate) compaction: CompactionDisposition,
}

pub(crate) fn plan(input: ContextInput) -> ContextPlan {
    let fixed_input = input
        .system_bytes
        .saturating_add(input.tool_schema_bytes)
        .saturating_add(input.irreducible_message_bytes);
    let input_bytes = fixed_input.saturating_add(input.reducible_message_bytes);
    // Compatibility is established by the caller from effective model,
    // prompt/tool identity, and an append-only transcript watermark. When it
    // holds, the provider's measured occupancy is more useful than applying
    // the byte-ratio estimate to the unchanged prefix.
    let estimated_input_tokens = input
        .compatible_input_tokens
        .unwrap_or_else(|| estimate_tokens(input_bytes));
    let output_tokens = u64::from(input.max_output_tokens);
    let storage_reserve_bytes = output_tokens
        .saturating_mul(CONSERVATIVE_OUTPUT_BYTES_PER_TOKEN)
        .saturating_add(if input.compaction == CompactionDisposition::Eligible {
            COMPACTION_STORAGE_ENVELOPE_BYTES
        } else {
            0
        });
    let fixed_tokens = estimate_tokens(fixed_input).saturating_add(output_tokens);
    let required_tokens = estimated_input_tokens.saturating_add(output_tokens);
    let estimate = ContextEstimate {
        input_bytes,
        estimated_input_tokens,
        output_reserve_tokens: output_tokens,
        storage_reserve_bytes,
        context_window: input.context_window,
    };

    let exceeds_storage = input_bytes.saturating_add(storage_reserve_bytes) > STORAGE_CONTEXT_BYTES;
    // The summarizer reads the very transcript that overflowed; judging it
    // against the model window would refuse every compaction a window
    // overflow asked for. The provider adjudicates that request; only the
    // storage backstop is planned here.
    let exceeds_window = input.compaction != CompactionDisposition::Summarizing
        && input
            .context_window
            .is_some_and(|window| required_tokens > u64::from(window));
    // An eligible prompt run compacts before it crosses the window, while the
    // summarizer still has room; nothing else (mid-run turns, already
    // attempted, the summarizer) is held to the headroom.
    let near_window = input.compaction == CompactionDisposition::Eligible
        && input.reducible_message_bytes > 0
        && input.context_window.is_some_and(|window| {
            let window = u64::from(window);
            required_tokens > window.saturating_sub(window / PROACTIVE_COMPACTION_HEADROOM_DIVISOR)
        });
    // A compatible provider measurement covers the complete prior request.
    // Let a measured fit win before classifying raw byte weights as an
    // irreducible model-window overflow; the byte storage backstop remains
    // independently authoritative.
    if !exceeds_storage && !exceeds_window && !near_window {
        return ContextPlan::Send { estimate };
    }

    let irreducible_window_overflow = input.compatible_input_tokens.is_none()
        && input.compaction != CompactionDisposition::Summarizing
        && input
            .context_window
            .is_some_and(|window| fixed_tokens > u64::from(window));
    let irreducible_storage_overflow =
        fixed_input.saturating_add(storage_reserve_bytes) > STORAGE_CONTEXT_BYTES;
    if irreducible_window_overflow || irreducible_storage_overflow {
        let constraint = match (irreducible_window_overflow, irreducible_storage_overflow) {
            (true, true) => ContextConstraint::Both,
            (true, false) => ContextConstraint::ModelWindow,
            (false, true) => ContextConstraint::StorageBackstop,
            (false, false) => unreachable!(),
        };
        return ContextPlan::Reject {
            estimate,
            reason: ContextRejectReason::Irreducible(constraint),
        };
    }
    let constraint = match (exceeds_window || near_window, exceeds_storage) {
        (true, true) => ContextConstraint::Both,
        (true, false) => ContextConstraint::ModelWindow,
        (false, true) => ContextConstraint::StorageBackstop,
        (false, false) => unreachable!(),
    };
    let target = ContextTarget {
        max_reducible_input_tokens: match (input.compatible_input_tokens, input.context_window) {
            (None, Some(window)) => Some(
                u64::from(window)
                    .saturating_sub(output_tokens)
                    .saturating_sub(estimate_tokens(fixed_input)),
            ),
            (None, None) | (Some(_), _) => None,
        },
        max_reducible_input_bytes: STORAGE_CONTEXT_BYTES
            .saturating_sub(storage_reserve_bytes)
            .saturating_sub(fixed_input),
    };
    if input.reducible_message_bytes == 0 {
        return ContextPlan::Reject {
            estimate,
            reason: ContextRejectReason::NoReducibleHistory(constraint),
        };
    }
    match input.compaction {
        CompactionDisposition::Eligible => ContextPlan::Compact {
            estimate,
            reason: constraint,
            target,
        },
        CompactionDisposition::AlreadyAttempted | CompactionDisposition::Summarizing => {
            ContextPlan::Reject {
                estimate,
                reason: ContextRejectReason::AlreadyAttempted(constraint),
            }
        }
        CompactionDisposition::BetweenRunsOnly => ContextPlan::Reject {
            estimate,
            reason: ContextRejectReason::BetweenRunsOnly(constraint),
        },
        CompactionDisposition::Unsupported => ContextPlan::Reject {
            estimate,
            reason: ContextRejectReason::Unsupported(constraint),
        },
    }
}

pub(crate) fn rejection_message(plan: ContextPlan) -> Option<String> {
    let (estimate, constraint, detail) = match plan {
        ContextPlan::Send { .. } => return None,
        ContextPlan::Compact {
            estimate,
            reason,
            target,
        } => {
            let token_target = target
                .max_reducible_input_tokens
                .map_or_else(|| "unknown".to_owned(), |target| target.to_string());
            let detail = format!(
                "automatic compaction must reduce reducible history to at most {token_target} estimated tokens and {} bytes before this request can start",
                target.max_reducible_input_bytes,
            );
            (estimate, reason, detail)
        }
        ContextPlan::Reject { estimate, reason } => {
            let (constraint, detail) = match reason {
                ContextRejectReason::Irreducible(constraint) => (
                    constraint,
                    "the system prompt, tool schemas, and this run's own messages alone exceed the limit, so compaction cannot help; shorten the prompt or instructions, or start a new session".to_owned(),
                ),
                ContextRejectReason::NoReducibleHistory(constraint) => (
                    constraint,
                    "no earlier history remains to compact; start a new session".to_owned(),
                ),
                ContextRejectReason::AlreadyAttempted(constraint) => (
                    constraint,
                    "one automatic compaction already ran for this prompt and the context is still too large; run /compact again or start a new session".to_owned(),
                ),
                ContextRejectReason::BetweenRunsOnly(constraint) => (
                    constraint,
                    "the context grew past the limit during this run and compaction runs only between prompts; run /compact or start a new session, then retry".to_owned(),
                ),
                ContextRejectReason::Unsupported(constraint) => (
                    constraint,
                    "the direct `qq ask` path has no session to compact; use a durable session (TUI or `qq run --session`) for long conversations".to_owned(),
                ),
                ContextRejectReason::ProviderReportedOverflow => {
                    return Some(format!(
                        "the provider previously rejected an equivalent request for exceeding its context window, and the single automatic compaction attempt did not produce a usable smaller context; the current provider-neutral estimate is {} input tokens with a {}-token output reserve; run /compact or start a new session",
                        estimate.estimated_input_tokens, estimate.output_reserve_tokens,
                    ));
                }
            };
            (estimate, constraint, detail)
        }
    };
    Some(match constraint {
        ContextConstraint::ModelWindow => format!(
            "context is estimated at {} input tokens ({} bytes) plus a {}-token output reserve, over the selected model's {}-token window; {detail}",
            estimate.estimated_input_tokens,
            estimate.input_bytes,
            estimate.output_reserve_tokens,
            estimate.context_window.unwrap_or(0),
        ),
        ContextConstraint::StorageBackstop => format!(
            "context measures {} bytes and reserves {} bytes for bounded output and compaction headroom, over the {} MiB per-session storage limit; {detail}",
            estimate.input_bytes,
            estimate.storage_reserve_bytes,
            STORAGE_CONTEXT_BYTES / (1024 * 1024),
        ),
        ContextConstraint::Both => format!(
            "context is estimated at {} input tokens plus a {}-token output reserve against the selected model's {}-token window and measures {} bytes with {} bytes of bounded output and compaction headroom against the {} MiB per-session storage limit; {detail}",
            estimate.estimated_input_tokens,
            estimate.output_reserve_tokens,
            estimate.context_window.unwrap_or(0),
            estimate.input_bytes,
            estimate.storage_reserve_bytes,
            STORAGE_CONTEXT_BYTES / (1024 * 1024),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte fixtures are multiples of the ratio so token arithmetic reads
    /// directly: `bytes(n)` bytes estimate to exactly `n` tokens.
    const fn bytes(tokens: u64) -> u64 {
        tokens * ESTIMATED_BYTES_PER_TOKEN
    }

    fn input(context_window: Option<u32>) -> ContextInput {
        ContextInput {
            context_window,
            max_output_tokens: 40,
            system_bytes: bytes(10),
            tool_schema_bytes: bytes(10),
            reducible_message_bytes: bytes(20),
            irreducible_message_bytes: bytes(20),
            compatible_input_tokens: None,
            compaction: CompactionDisposition::Eligible,
        }
    }

    fn assert_send(plan: ContextPlan) {
        assert!(matches!(plan, ContextPlan::Send { .. }), "{plan:?}");
    }

    fn assert_compact(plan: ContextPlan) {
        assert!(matches!(plan, ContextPlan::Compact { .. }), "{plan:?}");
    }

    fn assert_reject(plan: ContextPlan) {
        assert!(matches!(plan, ContextPlan::Reject { .. }), "{plan:?}");
    }

    #[test]
    fn tokens_are_estimated_at_four_bytes_each_rounded_up() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(estimate_tokens(729_498 * 4), 729_498);
        assert_eq!(estimate_tokens(u64::MAX), u64::MAX / 4 + 1);
        // The user-visible failure this ratio fixes: ~730 KB of transcript
        // on a 200k-token model is ~182k tokens and must send.
        let ContextPlan::Send { estimate } = plan(ContextInput {
            context_window: Some(200_000),
            max_output_tokens: 16_384,
            system_bytes: 20_000,
            tool_schema_bytes: 12_000,
            reducible_message_bytes: 680_000,
            irreducible_message_bytes: 17_498,
            compatible_input_tokens: None,
            compaction: CompactionDisposition::AlreadyAttempted,
        }) else {
            panic!("a 730 KB transcript fits a 200k window at four bytes per token")
        };
        assert_eq!(estimate.estimated_input_tokens, 182_375);
    }

    #[test]
    fn known_window_exact_fit_sends_and_one_token_over_compacts() {
        // Exact fit is judged without the proactive headroom: a run that
        // already compacted once sends at the window.
        let mut attempted = input(Some(100));
        attempted.compaction = CompactionDisposition::AlreadyAttempted;
        let ContextPlan::Send { estimate } = plan(attempted) else {
            panic!("exact fit must send")
        };
        assert_eq!(estimate.estimated_input_tokens, 60);
        assert_eq!(estimate.output_reserve_tokens, 40);
        assert_eq!(estimate.context_window, Some(100));
        assert_eq!(estimate.input_bytes, bytes(60));
        let ContextPlan::Compact {
            estimate,
            reason,
            target,
        } = plan(input(Some(99)))
        else {
            panic!("one token over must compact")
        };
        assert_eq!(reason, ContextConstraint::ModelWindow);
        assert_eq!(target.max_reducible_input_tokens, Some(19));
        assert_eq!(
            target.max_reducible_input_bytes,
            STORAGE_CONTEXT_BYTES - bytes(40) - estimate.storage_reserve_bytes
        );
    }

    #[test]
    fn tool_schema_and_output_reserve_are_both_part_of_the_window() {
        let mut request = input(Some(100));
        request.system_bytes = bytes(10);
        request.tool_schema_bytes = bytes(30);
        request.reducible_message_bytes = 0;
        request.irreducible_message_bytes = bytes(20);
        assert_send(plan(request));

        request.tool_schema_bytes = bytes(30) + 1;
        assert_reject(plan(request));
    }

    #[test]
    fn one_transcript_rejects_compacts_or_sends_for_three_windows() {
        let request = ContextInput {
            context_window: Some(99),
            max_output_tokens: 40,
            system_bytes: bytes(30),
            tool_schema_bytes: bytes(10),
            reducible_message_bytes: bytes(100),
            irreducible_message_bytes: bytes(20),
            compatible_input_tokens: None,
            compaction: CompactionDisposition::Eligible,
        };
        assert_reject(plan(request));
        assert_compact(plan(ContextInput {
            context_window: Some(150),
            ..request
        }));
        // 200 tokens required of a 200 window: inside the window but not the
        // proactive headroom, so an eligible run compacts first...
        assert_compact(plan(ContextInput {
            context_window: Some(200),
            ..request
        }));
        // ...and sends once the window leaves ten percent free.
        assert_send(plan(ContextInput {
            context_window: Some(223),
            ..request
        }));
    }

    #[test]
    fn eligible_runs_compact_proactively_inside_the_last_tenth_of_the_window() {
        // 60 input + 40 output = 100 required. Headroom on a 110 window is
        // 11, so 100 > 99 compacts while 100 <= 108 on a 120 window sends.
        let eligible = input(Some(110));
        let ContextPlan::Compact { reason, target, .. } = plan(eligible) else {
            panic!("an eligible run inside the headroom must compact")
        };
        assert_eq!(reason, ContextConstraint::ModelWindow);
        assert_eq!(target.max_reducible_input_tokens, Some(30));
        assert_send(plan(input(Some(120))));
        // Only a first-chance prompt run is held to the headroom.
        for disposition in [
            CompactionDisposition::AlreadyAttempted,
            CompactionDisposition::BetweenRunsOnly,
            CompactionDisposition::Unsupported,
            CompactionDisposition::Summarizing,
        ] {
            assert_send(plan(ContextInput {
                compaction: disposition,
                ..eligible
            }));
        }
        // Nothing reducible: sending is the only option short of rejecting.
        assert_send(plan(ContextInput {
            reducible_message_bytes: 0,
            irreducible_message_bytes: bytes(40),
            ..eligible
        }));
    }

    #[test]
    fn the_summarizer_request_is_planned_against_storage_only() {
        // The transcript that overflowed a 150-token window is exactly what
        // the summarizer must read: planning it against the window would
        // refuse every window-triggered compaction.
        let overflowing = ContextInput {
            context_window: Some(150),
            max_output_tokens: 40,
            system_bytes: bytes(30),
            tool_schema_bytes: 0,
            reducible_message_bytes: bytes(400),
            irreducible_message_bytes: bytes(20),
            compatible_input_tokens: None,
            compaction: CompactionDisposition::AlreadyAttempted,
        };
        assert_reject(plan(overflowing));
        assert_send(plan(ContextInput {
            compaction: CompactionDisposition::Summarizing,
            ..overflowing
        }));
        // Even a fixed prefix past the window is the provider's call.
        assert_send(plan(ContextInput {
            system_bytes: bytes(400),
            compaction: CompactionDisposition::Summarizing,
            ..overflowing
        }));
        // The storage backstop still binds the summarizer.
        assert_reject(plan(ContextInput {
            reducible_message_bytes: STORAGE_CONTEXT_BYTES,
            compaction: CompactionDisposition::Summarizing,
            ..overflowing
        }));
    }

    #[test]
    fn unknown_windows_still_obey_the_independent_storage_backstop() {
        let storage_reserve =
            COMPACTION_STORAGE_ENVELOPE_BYTES + CONSERVATIVE_OUTPUT_BYTES_PER_TOKEN;
        let exact = ContextInput {
            context_window: None,
            max_output_tokens: 1,
            system_bytes: 0,
            tool_schema_bytes: 0,
            reducible_message_bytes: STORAGE_CONTEXT_BYTES - storage_reserve - 1,
            irreducible_message_bytes: 1,
            compatible_input_tokens: None,
            compaction: CompactionDisposition::Eligible,
        };
        assert_send(plan(exact));
        assert_compact(plan(ContextInput {
            reducible_message_bytes: STORAGE_CONTEXT_BYTES - storage_reserve,
            ..exact
        }));
        assert_reject(plan(ContextInput {
            system_bytes: STORAGE_CONTEXT_BYTES + 1,
            reducible_message_bytes: 0,
            irreducible_message_bytes: 0,
            ..exact
        }));
        assert_reject(plan(ContextInput {
            reducible_message_bytes: STORAGE_CONTEXT_BYTES,
            compaction: CompactionDisposition::AlreadyAttempted,
            ..exact
        }));
    }

    #[test]
    fn compatible_usage_reuses_measured_occupancy_but_incompatible_input_does_not() {
        let request = input(Some(120));
        assert_send(plan(request));
        assert_compact(plan(ContextInput {
            compatible_input_tokens: Some(91),
            ..request
        }));
        let byte_heavy = ContextInput {
            context_window: Some(150),
            max_output_tokens: 40,
            system_bytes: bytes(10),
            tool_schema_bytes: bytes(10),
            reducible_message_bytes: bytes(120),
            irreducible_message_bytes: bytes(20),
            compatible_input_tokens: None,
            compaction: CompactionDisposition::Eligible,
        };
        assert_compact(plan(byte_heavy));
        assert_send(plan(ContextInput {
            compatible_input_tokens: Some(70),
            ..byte_heavy
        }));

        let raw_fixed_prefix_exceeds_the_window = ContextInput {
            context_window: Some(100),
            max_output_tokens: 40,
            system_bytes: bytes(80),
            tool_schema_bytes: 0,
            reducible_message_bytes: bytes(100),
            irreducible_message_bytes: 0,
            compatible_input_tokens: Some(10),
            compaction: CompactionDisposition::AlreadyAttempted,
        };
        assert_send(plan(raw_fixed_prefix_exceeds_the_window));
        let ContextPlan::Compact { target, .. } = plan(ContextInput {
            compatible_input_tokens: Some(70),
            compaction: CompactionDisposition::Eligible,
            ..raw_fixed_prefix_exceeds_the_window
        }) else {
            panic!("compatible occupancy cannot prove the reducible prefix is irreducible")
        };
        assert_eq!(target.max_reducible_input_tokens, None);
    }

    #[test]
    fn rejection_messages_name_a_recovery_step() {
        let mut request = input(Some(99));
        request.compaction = CompactionDisposition::AlreadyAttempted;
        let message = rejection_message(plan(request)).unwrap();
        assert!(message.contains("/compact"), "{message}");
        assert!(
            message.contains("estimated at 60 input tokens"),
            "{message}"
        );
        request.compaction = CompactionDisposition::BetweenRunsOnly;
        assert!(
            rejection_message(plan(request))
                .unwrap()
                .contains("/compact")
        );
        request.compaction = CompactionDisposition::Unsupported;
        assert!(rejection_message(plan(request)).unwrap().contains("qq ask"));
        request.compaction = CompactionDisposition::Eligible;
        request.reducible_message_bytes = 0;
        request.irreducible_message_bytes = bytes(40);
        assert!(
            rejection_message(plan(request))
                .unwrap()
                .contains("new session")
        );
    }

    #[test]
    fn all_capacity_arithmetic_saturates_closed() {
        assert_reject(plan(ContextInput {
            context_window: Some(u32::MAX),
            max_output_tokens: u32::MAX,
            system_bytes: u64::MAX,
            tool_schema_bytes: u64::MAX,
            reducible_message_bytes: u64::MAX,
            irreducible_message_bytes: u64::MAX,
            compatible_input_tokens: Some(u64::MAX),
            compaction: CompactionDisposition::Eligible,
        }));
    }
}
