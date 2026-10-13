# QQ accepted recovery integration

This candidate is a mechanical composition from approved base
`8591c705cf97ab4d38ea6dc0b4894e0af15aacd0`:

- auth R3: `d04bc590bf759f818f6f173336df939a5981169e`
- JD1: `0dfe4cf29bf58560b4d1c0c1e6d6b312342fee7b`
- TB1: `05c0772bf82a97e7161e525509be84ac0a300087`

Each complete accepted range was cherry-picked with `-x`, preserving source
commit provenance. Auth remains Keyring-only memoization with warm permission
checks. JD1 remains criterion-level validated diagnostics in the existing
feedback/event path. TB1 carries one typed remaining tool allowance from the
budget meter through dispatch, rejects excess calls before effects, and keeps
nonrejected charging and the final-response boundary unchanged.

The recorded composition evidence is
`source-composition.cast`, retained outside the source checkout.
It contains the actual cherry-pick commands, accepted-path allowlist check,
tip-to-integrated blob equality checks, `git diff --check`, and clean-source
readback. No tests, build, provider, credential, signing, installation, or
live-runtime operation belongs to this source composition step.
