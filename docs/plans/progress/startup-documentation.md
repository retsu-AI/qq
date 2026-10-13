# Startup documentation delivery ledger

| Slice | Goal | Status | Branch or PR | Next action |
| --- | --- | --- | --- | --- |
| DOCS1 | Source-backed startup guides in the existing QQ website | Integrated candidate | `docs/startup-systems-20261013` | Verify this integrated candidate, obtain final independent acceptance, and publish the reviewable PR |

## 2026-10-13

Existing website identified and isolated branch created from QQ `9bae6937b50a013f652db8963ac3590103eb4b84`. The shared source checkout contains unrelated edits and was preserved. Documentation specification written before site implementation. One site writer owns new guides and navigation. The separate master architecture owner owns only `website/public/architecture/`. No merge, deployment, product acceptance, or new unit or integration tests occurred.

The writer committed thirteen guide pages and navigation in `21e62ad15`, then expanded source references and feature coverage in `74aa6c9b2`. The site build passed for that content revision. `3d9e47ede` added the header entry and eight semantic task cards. Its build passed, with 28 generated pages and 1,398 valid internal links. Ten actual Chromium workflow groups passed for that identity, covering navigation, all thirteen startup pages, search, deep links, keyboard use, themes, focus, missing-page behavior, and mobile layout. Browser screenshots, video, and raw results are retained privately by the documentation owner. They do not establish application behavior or deployment.

The independent non-author source review returned `FOR_REVISION` for `74aa6c9b2`. Its four blockers cover an incorrect headless cost field, stale Retsu authentication and monitoring details, an ambiguous capability endpoint observation, and the same-app native mode contract. The same writer owns repairs. The documentation owner refreshed Retsu remote `main` to `8d58080829231bb74e180c6de4e8f687c4606200` for current source documentation and preserved the older local checkout. The reviewer uses the requested Fusion pair through a separate read-only session. No new provider plan, account configuration, unit or integration tests, merge, or deployment was performed.

### Integrated candidate checkpoint

The same writer repaired the source findings in `7826093f3` and mapped the current implemented Retsu SDK, model, product documentation, OpenAPI, and web sources in `1744ef703`. The assistant source pin is corrected and the normal Rust and website verification paths remain distinct. The existing Retsu product docs remain their own authoritative source; the QQ startup hub links them.

The architecture owner published [PR #288](https://github.com/retsu-AI/qq/pull/288) at `fa9556f8a2845e8a5d06bda7c77f186185d0cd57`, with independent `ACCEPT_VISUAL_SCOPED`. Its four commits are incorporated without asset edits. The 55-component, 63-relationship registry distinguishes Better Auth/D1 identity, separate AccountLedger throttle counter instances, customer money, Billing webhook orchestration, monitoring cron, and the hosted Temporal target. Overview navigation opens the supplied module.

This checkpoint precedes final integrated build, browser evidence, and content acceptance. Those receipts and the eventual PR postcondition are retained by the documentation delivery owner outside the public repository. Architecture acceptance does not establish site-wide acceptance, hosted CI, merge, deployment, provider execution, payment, or customer acceptance.

### Final repair checkpoint

The integrated candidate `94ae7e27b` passed the static build (28 pages, 1,404 links), secret scan, and ten guide/browser groups plus a focused architecture check. Failed driver selectors and a literal heading-whitespace assertion remain in the private evidence; their repairs were checked without relabeling the failed runs. Actual recordings and browser-close receipts are retained. The independent review resolved the original source blockers and identified one remaining reader instruction error: website build and Rust docs-truth checks had been conflated.

The same writer repaired that error and the finite source wording findings in `15f531b3c`. The header now marks only the active navigation group. Final verification is limited to the changed rendered guidance, header navigation, recovery link, and the previously screenshot-only architecture mobile interaction; earlier unchanged browser groups retain their original subject. The same reviewer owns the final changed-head rebind. Publication and hosted-check receipts are recorded outside this dated pre-publication source checkpoint.
