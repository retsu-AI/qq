# QQ Domain Language

Some terms name planned concepts with no implementation yet: Workspace Root,
Workspace Provisioning, Change Set, Client Enrollment, Pairing Code, Client
Credential, Server Profile, Multi-Server Overview, Client Cache, Fold/Focus,
Execution Checkout and Repository Publication (see the [desktop, mobile and
fleet plan](docs/plans/desktop-mobile-fleet.md)). Today every run executes in
the session's working directory, the user's checkout.

## Terms

### Server

A QQ installation with a stable identity that owns authoritative workspace,
session, and run state. Its identity survives process restarts and endpoint
changes.

### Workspace

The codebase scope managed by a QQ server. A workspace contains root sessions
and their descendants.

### Workspace Root

A server-configured directory that bounds workspace discovery and creation for
remote clients. It contains workspaces but is not itself a workspace.

### Workspace Provisioning

The creation of a workspace beneath a workspace root by cloning an existing
repository or initializing a new local Git repository.

### Workspace Catalog

A server's set of known workspaces available for selection. Workspaces enter
the catalog through local use, explicit addition, or provisioning.

### Execution Checkout

The filesystem view of a workspace used by a run. An isolated checkout changes
where a child executes without creating a different workspace.

### Change Set

A durable proposal of workspace changes produced in an isolated execution
checkout and tied to its base revision. It may be reviewed, integrated, or
rejected without transferring the child transcript.

### Repository Publication

The externally visible act of creating or updating a hosted repository from a
local workspace. It is separate from workspace provisioning.

### Session

A durable, focusable conversation in a workspace. A session may be a root or
have one parent session, can run independently of other sessions, and outlives
any attached client.

### Root Session

A session with no parent. Starting independent work creates a root session.

_Avoid_: Job

### Child Session

A session created in relation to one parent session. Child sessions retain
their own conversation and may run concurrently with their parent and siblings.

### Coordinator

The role of an agent run that delegates scoped tasks to child runs and
synthesizes their results. It is not a separate agent type.

### Delegation

A coordinator gives one self-contained brief to a child run and receives one
final result. Child agents do not communicate directly or receive mid-run
steering.

### Session Tree

A root session and all of its descendants, owned by one server and contained in
one workspace.

### Prompt

User input submitted to one session. Prompts are ordered within that session.

### Follow-Up

A prompt submitted while its session already has an active run. It waits in
that session's queue and starts after earlier prompts finish.

### Transcript

The authoritative ordered account of prompts, assistant turns, tool calls, and
tool results within a session. It is not a message-only chat log.

### Run

The execution of one prompt within a session. A session has at most one active
run, while different sessions may have active runs concurrently.

### Run Tree

A root run and every child run it delegates. Cancellation and inclusive limits
apply to the run tree as one execution boundary.

_Avoid_: Swarm

### Run Budget

Optional time, model-turn, and cost limits for one run tree.

### Needs Attention

A live state in which execution is blocked awaiting an explicit operator
action. It does not mean failed, completed, or unread.

### Client

An attached TUI, CLI, mobile app, or future interface. Clients may attach to
multiple servers and share control of durable sessions, but do not own work or
coordination.

### Client Enrollment

A server's durable authorization for one remote client. Each enrollment has
independent credentials and can be revoked without affecting other clients.

_Avoid_: Registration

### Pairing Code

A short-lived, single-use grant that permits one client enrollment with a
server.

### Client Credential

The secret proof issued for one client enrollment. Revoking the enrollment
invalidates only that credential.

### Server Profile

A client's saved reference to one enrolled server. It is keyed by the server's
stable identity rather than its current network endpoint.

### Multi-Server Overview

A client view that combines connectivity and activity summaries from enrolled
servers. It does not coordinate or own work across servers.

### Client Cache

A bounded, non-authoritative client projection of recent server state. Cached
state may be shown as stale but never owns work or queues commands.

### Threadline

A TUI view of a session tree and its concurrent activity. Threadline is not a
separate persisted domain object.

### Fold/Focus

A TUI view that condenses inactive history and emphasizes the focused session
and current activity. Fold/Focus uses the same session state as Threadline.
