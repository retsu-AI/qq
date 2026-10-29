# syntax=docker/dockerfile:1
#
# QQ in a container with only the project mounted. Commands the agent runs
# reach this container's files and network and the credentials you pass in,
# not your machine's. See docs/guide/running-unattended.md.
#
#   docker build -t qq .
#   docker run --rm -it -v "$PWD:/workspace" -e ANTHROPIC_API_KEY qq
#   docker run -d --name qq -v "$PWD:/workspace" -e ANTHROPIC_API_KEY qq daemon run
#
# The default (last) stage is that runtime image. The `dev` stage is this
# repository's Rust toolchain, for .devcontainer/.

ARG RUST_VERSION=1.97.1
ARG DEBIAN=trixie

FROM rust:${RUST_VERSION}-${DEBIAN} AS dev
ARG UID=1000
ARG GID=1000
COPY rust-toolchain.toml /tmp/toolchain/
RUN cd /tmp/toolchain && rustup toolchain install && rm -rf /tmp/toolchain \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"
RUN groupadd --gid "$GID" agent \
    && useradd --create-home --uid "$UID" --gid "$GID" --shell /bin/bash agent
USER agent
# Container builds keep their own target directory so they never overwrite
# the host's build in the mounted checkout; `cargo build` puts `qq` on PATH.
ENV CARGO_TARGET_DIR=/home/agent/target \
    PATH=/home/agent/target/debug:$PATH \
    GIT_TERMINAL_PROMPT=0 \
    USER=agent

FROM rust:${RUST_VERSION}-${DEBIAN} AS build
ARG RUST_VERSION
# The image's toolchain; the binary needs none of rust-toolchain.toml's
# extra components or targets.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release --bin qq \
    && install -m 0755 target/release/qq /usr/local/bin/qq

FROM debian:${DEBIAN}-slim AS qq
ARG UID=1000
ARG GID=1000
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git tini \
    && rm -rf /var/lib/apt/lists/*
RUN groupadd --gid "$GID" agent \
    && useradd --create-home --uid "$UID" --gid "$GID" --shell /bin/bash agent
COPY --from=build /usr/local/bin/qq /usr/local/bin/qq
USER agent
WORKDIR /workspace
ENV GIT_TERMINAL_PROMPT=0 \
    USER=agent
# tini reaps processes that commands leave behind when qq is PID 1.
ENTRYPOINT ["tini", "--", "qq"]
