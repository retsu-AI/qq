# Retained installer feature map

The installer publishes checksum-identified executable payloads and atomically
selects a relative symlink. It preserves legacy rollback bytes before replacing
an idle regular executable. Retained files are verified single-link executables
with mode `0555`; the owner can still deliberately change permissions.

| Journey | Source | Verification |
|---|---|---|
| Retain and select a new executable | `install.sh` | Existing credential-free native-process fixture executes old and new payloads |
| Migrate a legacy executable and roll back | `install.sh` | Actual retained legacy checksum and rollback execution |
| Refuse busy, unexpected or corrupt targets | `install.sh` | Existing busy-file, special-node, foreign-link and checksum refusal cases |
| Preserve read-only single-link payloads | `install.sh` | Mode and hard-link checks in the native fixture |
| Save input and reopen a session | `docs/guide/install.md` | Operator procedure and separate live acceptance |

The macOS fixture uses real compiled child processes, `proc_pidpath` and `lsof`.
Its revised probe recorded twelve successful commands and twelve intentional
failure commands, followed by owned cleanup. Shell syntax, ShellCheck and
whitespace validation apply to the reviewed source revision.

Linux execution, device-node fixtures, malicious races by the same user, live QQ
installation, signing, Keychain persistence and provider/session acceptance are
separate limits. Source checks establish installer behavior only.
