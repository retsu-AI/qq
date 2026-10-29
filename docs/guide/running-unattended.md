# Running unattended

QQ runs the agent's commands in your checkout, as you, with your network
and your credentials ([Permissions › Where commands
run](permissions.md#where-commands-run)). Sitting at the TUI, you see each
step. A goal left running overnight, a daemon, or a CI job has no one
watching, so what the commands can reach is up to the machine QQ runs on.
QQ does not sandbox anything itself. This page shows how to set up that
machine.

`qq doctor` tells you which setup you are in:

```
ok    environment      in a container (/.dockerenv): commands run as agent, with the container's network and the credentials passed into it
ok    snapshots        on: `qq undo` restores the workspace as it was before each run
```

On the host it says `on the host: commands run as you, with this machine's
network and credentials`, and `qq run --goal` prints a one-line note on
stderr pointing here. The note never waits for an answer.

None of this is a guarantee. A container limits what a mistake or a
prompt-injected command can reach; it does not make the agent's choices
correct. Review what it pushes.

## Mount only the project

Run QQ in a container that sees the project and nothing else from your
machine: no home directory, no `~/.ssh`, no Docker socket. This repository
ships a `Dockerfile` that does this:

```sh
docker build -t qq .
docker run --rm -it -v "$PWD:/workspace" -e ANTHROPIC_API_KEY qq
```

`-v "$PWD:/workspace"` is the only mount. The image runs as an unprivileged
`agent` user with `git` installed, so snapshots and `qq undo` work on the
mounted checkout. Everything after the image name is passed to `qq`:

```sh
docker run --rm -v "$PWD:/workspace" -e ANTHROPIC_API_KEY qq run --goal "Make cargo test pass" --check 'cargo test' --format jsonl
```

For a long-lived server, run the daemon and attach to it inside the
container:

```sh
docker run -d --name qq -v "$PWD:/workspace" -v qq-data:/home/agent/.local/share/qq -e ANTHROPIC_API_KEY qq daemon run
docker exec -it qq qq
```

The daemon binds loopback inside the container, so nothing outside it can
reach the API. The `qq-data` volume keeps sessions and trust records when
the container is replaced; without it they go with the container.

The runtime image has only `git` and QQ. Build your own image `FROM qq` with
the tools your project needs (a compiler, `node`, `gh`), or use the
devcontainer below.

### The devcontainer

`.devcontainer/devcontainer.json` builds the `Dockerfile`'s `dev` stage:
this repository's Rust toolchain, the checkout as the only mount, and `qq`
built from it on `PATH`. Open the folder in an editor that supports dev
containers, or use the CLI:

```sh
npx @devcontainers/cli up --workspace-folder .
npx @devcontainers/cli exec --workspace-folder . qq
```

Editors forward some host integrations into a dev container by default.
The configuration clears `SSH_AUTH_SOCK`, so the agent's commands cannot
use your SSH keys. VS Code also copies your `~/.gitconfig` and forwards its
Git credential helper; turn those off with `dev.containers.copyGitConfig:
false` and `dev.containers.gitCredentialHelperConfigLocation: "none"` in
your editor settings.

To adapt it for your own project, copy `.devcontainer/` and replace the
`dev` stage with your project's toolchain.

## Give each repository its own token

The container should hold only the credentials the task needs: the model
provider's key, and a token for the one repository the agent works on.

On GitHub, create a [fine-grained personal access
token](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens#creating-a-fine-grained-personal-access-token)
with:

- **Repository access:** only this repository;
- **Permissions:** Contents read and write, and Pull requests read and write
  if the agent opens PRs; nothing else;
- **Expiration:** as short as the work allows.

Pass it in as an environment variable, not a mounted file:

```sh
docker run --rm -it -v "$PWD:/workspace" -e ANTHROPIC_API_KEY -e GH_TOKEN qq
```

`gh` reads `GH_TOKEN` directly. For `git push` over HTTPS, have Git read it
too, in an image built `FROM qq`:

```sh
git config --global credential.https://github.com.helper '!f() { echo username=x-access-token; echo "password=$GH_TOKEN"; }; f'
```

Protect the default branch so the token cannot push to it, and let the
agent push to its own branches and open PRs instead. A token for one
repository that cannot merge limits the damage to one reviewable PR.

Never mount `~/.config/gh`, `~/.git-credentials`, `~/.aws` or `~/.ssh`. QQ's
[fixed stops](permissions.md#fixed-stops) refuse commands that name those
paths, but they only read literal commands, and a container that has no
such files is the real protection.

## Use a separate Git identity

Commit as the agent, not as you, so `git log` and blame show which changes
it made:

```sh
docker run --rm -it -v "$PWD:/workspace" -e ANTHROPIC_API_KEY -e GIT_AUTHOR_NAME="QQ agent" -e GIT_AUTHOR_EMAIL=agent@example.com -e GIT_COMMITTER_NAME="QQ agent" -e GIT_COMMITTER_EMAIL=agent@example.com qq
```

The container never sees your `~/.gitconfig`, so without these variables
Git refuses to commit. Don't give the agent your signing key. If your
repository requires signed commits, give the agent its own key, or a bot
account, and register that.

## Refuse commands with managed `deny_shell_prefixes`

Inside the container you are the administrator. Managed policy is read
from `/etc/qq/managed.ron` and must be owned by root, so bake it into an
image built from QQ's:

```ron managed.ron
policy: (
    deny_shell_prefixes: ["git push origin main", "gh pr merge", "gh release", "npm publish", "cargo publish"],
)
```

```dockerfile
FROM qq
USER root
COPY managed.ron /etc/qq/managed.ron
USER agent
```

These prefixes are refused in every approval mode, Full included, and the
agent gets a tool error telling it to find another way. Like the fixed
stops, they match literal commands at word boundaries; a script that runs
`git push` is not caught. Use them to block mistakes you can name, and use
the token's permissions and branch protection to make the rest impossible.
The key is described in [Configuration › Policy](configuration.md).

## Don't run unattended goals on a machine with production credentials

On the host, every command the agent runs can read what you can read: cloud
credentials in `~/.aws`, a `kubectl` context pointed at production, a
database URL in your shell's environment, SSH keys that reach servers. An
unattended goal runs for hours with no one reading each step, and one
command from a confused model or a prompt-injected file is enough to use
any of them. Snapshots and `qq undo` restore your files; they cannot undo a
deleted bucket, a dropped table, or a pushed secret.

So:

- run unattended goals in a container, a VM or a separate user account that
  has never held production credentials;
- don't add those credentials to the container "just for this task"; give it
  a narrower token, or do that step yourself;
- keep attended work, where you watch each step in the TUI, on your own
  machine if you like. The risk is the same, but you are there to see it.
