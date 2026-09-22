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

QQ runs model-directed code on your machine, so its security boundary is the
approval policy and the workspace containment around it. We treat these as
vulnerabilities:

- a tool call that escapes the workspace or the approval mode it ran under
  (path traversal, symlink escape, a `forbidden` shell shape that executes,
  a mutating action under `read_only`);
- credentials written to disk in plain text without `--allow-file`, sent to
  the wrong endpoint, or exposed in logs, events, or configuration output;
- project configuration influencing QQ before `qq trust` accepted it;
- the local server accepting requests it should refuse (cross-origin without
  `--allow-origin`, non-loopback without an authenticated client);
- `fetch` reaching private, link-local, or metadata addresses;
- prompt-injection paths where content the agent reads can change policy
  (rather than merely influence the model's choices within policy).

Not vulnerabilities on their own: a model choosing to do something unwise
within the approval mode you selected; `full` mode doing what it says;
denial of service by a provider.

## Supported versions

The latest release. Fixes land on `main` and ship in the next release; we do
not backport.

## Design references

[`docs/design/tools.md`](docs/design/tools.md) (containment, shell
classification, approval), ADR-0007, ADR-0020, ADR-0021, and
[`docs/guide/permissions.md`](docs/guide/permissions.md).
