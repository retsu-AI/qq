# ADR-0059: Declare the tools a run can execute and state its authority

**Status:** Proposed. Supersedes ADR-0054 section 5's read-only-root clause.
**Date:** 2026-10-14.

## Context

Unattended direct runs declared built-in mutations and commands although their
Ask gate could not approve them. Strict read-only declarations retained
External tools hinted read-only despite the gate denying every External call.
Read-only roots also received the implementation obligation from ADR-0054.

## Decision

Tool visibility uses one immutable eligibility predicate across base schemas,
pinned schemas, recovered pins, progressive indexes and selector reports.
Strict ReadOnly omits mutating, shell, network and every External tool,
including configured grants. Unattended Ask omits interactive questions and
keeps read-only builtins plus only configuration-granted External tools.
Approval decisions remain unchanged; host hints do not grant authority.

The plan compiler captures sorted configuration grants once. Catalog entries
record grant eligibility and direct gates use the same snapshot. Progressive
granted indexes are built off the request path. The Copy prefix key includes
immutable read-only and unattended dimensions and has at most 390 variants;
mutable approval modes remain outside prompt wording and cache identity.

A read-only root gives exact caller-applied edits or commands and stops when
a change is requested. An unattended root cannot ask questions, uses only
declared tools, may act through configured external grants, and returns exact
caller-applied changes if no declared tool can perform the task. Ordinary
interactive action wording remains unchanged. Read children retain their
answer-the-brief contract.

## Consequences

Embedders receive fewer schemas for calls that were already denied. This is a
visible compatibility change without a protocol or descriptor encoding bump.
A newly configured grant requires plan recompilation before direct execution;
request-time host configuration cannot change an existing plan's authority.

## Evidence

AB4's captured direct, granted/ungranted external and strict read-only recovery
fixtures, catalog selector/index tests and prompt identity tests record the
behavior. Qualification and performance receipts lived in the action-bias ledger,
removed in v0.1.10; see Git history.
