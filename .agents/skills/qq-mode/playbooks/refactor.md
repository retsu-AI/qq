# Refactor

You own the contract. The structure changes; the behavior does not. If
you find a bug or a missing feature, note it and ship it separately.

1. Pin the current behavior before you move anything. Find the tests that
   cover it. If none do, write a characterization test against the public
   API and see it pass. Type checks and lints are not a pin.
2. Name the target shape: the module layout, types, and call graph you
   would build today. The change must remove branches, layers, or invalid
   states. If it only moves them, stop.
3. Delete first: dead code, one-caller wrappers, redundant checks.
4. Move in small steps, each keeping the pin green. For an API change,
   migrate every caller and delete the old API in the same branch. No
   compatibility shims. Search for each renamed symbol in code, docs, and
   strings, not only in Rust paths.
5. Run the pin and the affected crate's tests. Load `review-diff` and
   confirm the diff is smaller or easier to follow. If it is not, revert.
6. If the user asked for a PR, load `open-pr`.

**Reply:** what changed in the structure, the tests that held the
behavior, and the net line count.
