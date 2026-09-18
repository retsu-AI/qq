# ADR-0033 — Keep model pins distinct from configured fallbacks

**Status:** Accepted
**Date:** 2026-09-18

Optional routing must preserve an explicit model choice. Comparing a session's
route with the configured default cannot establish intent: a user may explicitly
choose that same route. Model selections and session summaries therefore carry
`model_is_fallback`, defaulting to false for legacy clients and sessions.
Schema 32 persists this flag; migrating older sessions never enables routing.

CLI and environment model overrides, TUI picks, explicit child selections and
configured worker models are pins. A configured default is a fallback. The root
loader resolves a fallback from current configuration; it applies a pin as an
explicit override. A router cannot replace a pinned route, and its rejected
selection still contributes its actual spend to the run.

The flag records model-choice intent only. Independent trusted routing opt-in
remains required. This is part of the unpublished protocol 25 contract and
introduces no extra request, dependency or extension mechanism.
