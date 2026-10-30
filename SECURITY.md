# Security policy

## Reporting a vulnerability

Please do not open a public issue for security problems.

Use GitHub's private reporting: **Security → Report a vulnerability** on
<https://github.com/retsu-AI/qq/security/advisories/new>. You will get an
acknowledgement within three working days and a fix or a mitigation plan
within thirty for confirmed issues. We credit reporters in the release notes
unless you ask otherwise.

Include: the QQ version (`qq version`), platform, configuration shape (with
secrets redacted), and steps to reproduce.

## What counts

QQ runs model-directed code on your machine as you. Its security boundary is
the approval mode and the workspace containment of the built-in file tools;
shell commands run in your checkout with your user, network and credentials,
and are not contained. We treat these as vulnerabilities:

- a tool call that escapes the approval mode it ran under (a mutating or
  shell action under `read_only`, a non-read call under `ask` that runs
  without approval);
- a built-in file tool reaching outside the workspace (path traversal,
  symlink escape);
- credentials written to disk in plain text without `--allow-file`, sent to
  the wrong endpoint, or exposed in logs, events, or configuration output;
- project configuration influencing QQ before `qq trust` accepted it;
- the local server accepting requests it should refuse (cross-origin without
  `--allow-origin`, non-loopback without an authenticated client);
- `fetch` reaching private, link-local, or metadata addresses;
- prompt-injection paths where content the agent reads can change policy
  (rather than merely influence the model's choices within policy).

Not vulnerabilities on their own: a model choosing to do something unwise
within the approval mode you selected; `full` mode doing what it says; a shell
command reaching files, network or credentials you can reach; a script, an
interpreter or a command built at run time getting past a fixed stop or
`deny_shell_prefixes` (they match literal commands and are an accident net);
denial of service by a provider.

## Supported versions

The latest release. Fixes land on `main` and ship in the next release; we do
not backport.

## Design references

[`docs/design/tools.md`](docs/design/tools.md) (where commands run,
fixed stops, approval), ADR-0007, ADR-0021, and
[`docs/guide/permissions.md`](docs/guide/permissions.md).
