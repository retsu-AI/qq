# Protected credential reuse during local updates

## KP1 contract

Keep a working signed QQ credential authority usable across new sessions,
restarts, and local updates. Preserve the login Keychain items, their access
control, the original conversation, protocol 31, and store schema 43.
The source owner works on `fix/qq-keychain-persistence-20261013`, based on
`d377cf2e47628643130cf1ddc2820af4faf69bf5`. Root owns live runtime changes,
signing, publication, and acceptance.

## Established behavior

QQ uses the fixed Keychain service `dev.qq`. The account is the stored
credential name. Codex uses `openai-codex/{profile}`. Ordinary reads neither
create nor delete items. The macOS backend updates existing items in place.
The attached TUI reads index metadata and awaits server model selection.
The server shares one bounded coordinator across credential-store clones.
It coalesces concurrent reads and reuses successes for five minutes, while
the durable credential epoch invalidates replaced values.

The October 13 incident included queued dialogs for old requesters and two
successful current-server item approvals. The observed `-67065` is
`errSecCSNoSuchCode`, not the `-67034` disk-versus-running-code mismatch.
This does not establish that the current signed identity loses persistent
item authorization. A longer cache would hide the distinction.

## Smallest transition

Preserve each installed executable at a checksum-identified path. Select a
new version atomically without replacing any prior executable payload.
Do not prune a payload while a process may still use it. The initial move
from a regular installed executable requires a session-preserving quiescent
transition, since retaining a later copy cannot restore an old process's
original executable identity.

The data shape is an executable payload named by its SHA-256 and one selected
`qq` symlink. `install.sh` stages a new file on the destination filesystem,
creates the retained payload without overwriting an existing path, verifies an
existing same-hash payload, then renames the selection symlink atomically.
Signals remove only installer-owned staging files. Prior payloads are retained
release artifacts, not temporary build output. There is no automatic pruning.

A successful legacy regular-file migration first copies and verifies the old
executable into the same retained store on both supported operating systems.
It preserves both old and new payloads before replacing the selector. Inspection,
copy, checksum, or publication failure leaves the original selection unchanged.
Rollback can therefore select the old retained bytes after an idle migration.

Published payloads are mode `0555`. Existing payloads must be regular executable
files with the expected checksum and exactly one hard link; a verified writable
payload from an earlier installer is made read-only before selection. Legacy
regular executables must also have exactly one hard link. Additional links are
refused rather than changing permissions through an unknown alias. Temporary
publication links are removed before verification and selection; a concurrent
installer that observes that transient link may fail safely and be retried.

The selector accepts absence, a regular executable, or the installer's relative
`.qq-versions/qq-<sha256>` link to a verified retained payload. Other links,
directories, FIFOs, sockets, and devices are refused without replacement.
These checks prevent accidental mutation by this installer, not deliberate
tampering or filesystem races by the same user, who controls the directory and
can change permissions. Read-only retention is not an OS immutability guarantee.

Before replacing a regular macOS installation, `lsof` checks its current file
users. A busy or failed inspection leaves selection unchanged. The check cannot
prevent a concurrent new launch, identify already-exited requesters in an OS
dialog queue, or replace the transition owner's process reconciliation.

The transition owner records the original conversation and saves unsent
composer text before asking clients to exit. The owner settles or cancels
an active run, waits for durable idle state, closes the owned server, and
verifies that identified requesters exited before changing selection.
The same conversation is reopened after selection. An authorization dialog
for an exited requester is not evidence that the replacement lost access.

No new credential broker, wire format, database migration, authentication
flow, plaintext store, environment-secret fallback, or ACL mutation belongs
to this slice. No process is killed by an installer. Credential refresh
and its durable epoch/readback contract remain unchanged.

## Acceptance and recovery

Static validation covers the source and shell syntax. Any source-only
probe is labelled as such. The latest human instruction excludes new test
suites and unchanged suite reruns from this incident response.

Root verifies the original conversation's persisted rows and unsent input,
then observes a real completed provider response with existing credentials.
The same credential must work in a fresh session, after a controlled restart,
and after one signed update with the same designated requirement. Root
records actual Keychain prompt activity separately from provider completion.
The first repeated prompt stops the trial. No source check proves macOS
persistent authorization or guarantees that the OS never asks to unlock.

Rollback selects the retained prior signed artifact after the same quiescent
transition. Session data, backups, credential items, and diagnostic receipts
are preserved. The installer keeps prior payloads read-only and never overwrites
their bytes.
