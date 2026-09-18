# ADR-0032 — Routing spend belongs to the reserved run

**Status:** Accepted
**Date:** 2026-09-18

An optional task-routing decision happens before normal provider preparation.
Preparing first would freeze the wrong model, context limits, pricing and plan
identity; mutating those after `RunStarted` would break its existing contract.
The ordinary loader validates and compiles the selected route. Disabled runs
have no router and incur no routing requests or task projection.

Schema 31 records a pending routing marker on the queued reservation before
inference. Known usage/cost are persisted before loading a selected provider;
a bounded completed decision is committed with the protocol-25 routing event.
Duplicate dispatch, cancelled reservations and terminal writes are rejected.
Recovery interrupts routed reservations instead of automatically billing again.
An ordinary queued run without routing retains its previous recovery behavior.

Preparation failures and cancellation preserve known or unknown spend. The
runtime budget and durable accumulator each consume the same receipt once.
Auxiliary spend is distinct from a main-model turn and cannot clear context
occupancy. Compaction never routes or inherits the prompt's routing charge.

Production activation remains default-off and requires the concrete Jev adapter,
explicit-choice precedence and child-policy inheritance. This decision does not
turn credentials into consent, authorize tools, or claim a speed improvement.
