# Startup systems overview

This guide describes the current QQ and Retsu startup systems. It tells you
where to start and labels the boundary between source behavior, candidate work,
planned work, and runtime or deployment evidence.

## Choose a path

| You want to | Start here | Current basis |
| --- | --- | --- |
| Use the terminal agent | [Quickstart](quickstart.md) | Implemented QQ release behavior |
| Develop QQ | [Develop QQ](startup-qq-development.md) | QQ source and repository workflow |
| Use a native client | [Direct and assistant modes](startup-native-modes.md) | Candidate Mac and iOS client sources |
| Run an assistant gateway | [Assistant gateway and nodes](startup-assistant.md) | Candidate Rust assistant sources |
| Call the extraction API | [API and billing](startup-api-billing.md) | Separate Retsu API source |
| Operate a deployment | [Infrastructure](startup-infrastructure.md) | Plans and private infrastructure source |

The detailed QQ pages remain authoritative for released terminal behavior. Use
[Install](install.md), [Providers](providers.md), [CLI](cli.md), and
[Troubleshooting](troubleshooting.md) instead of copying those procedures here.

## System boundaries

QQ is one Rust binary. Its TUI, `qq ask`, `qq run`, and `qq serve` paths share
the agent runtime, provider adapters, tool executor, and SQLite session store.
The [architecture design](../design/architecture.md) records this source
boundary.

The assistant is a separate Rust project that uses QQ as its execution engine.
Its private store owns project bindings, memory, and pending intent metadata.
QQ owns credentials, sessions, transcripts, runs, provider calls, and tools.
The assistant source is a candidate and its provider and native acceptance are
unverified.

Retsu is a separate product repository. Its Grip service has a gateway and a
private model backend in its source design. The local private main snapshot is
not evidence of a deployed service. Repository access is required for that
source.

## Status vocabulary

- **Implemented source** means the checked source contains the behavior.
- **Candidate source** means a separate source unit describes or implements a
  slice that still needs composition or independent acceptance.
- **Planned** means the source or plan names work that is not implemented here.
- **Runtime or deployment unverified** means source and local checks do not
  prove a live provider, hosted service, native device, or customer journey.

Open the [feature map](startup-feature-map.md) for the source entrypoint and
status of each documented journey.
