# Linux e2e image for nerve; see tests/linux/e2e.sh and docs/TESTING.md.
# Pinned to the MSRV in Cargo.toml (rust-version).
FROM rust:1.85-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
      tmux procps xvfb xclip python3 \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy
# Stand-ins: a process whose comm is `claude`, a `claude` CLI that logs its
# argv, and a notify-send that records notifications.
RUN mkdir -p /fake && cp /bin/sleep /fake/claude-proc \
 && printf '#!/bin/sh\necho "$*" >> /tmp/claude-cli.log\nsleep 30\n' > /fake/claude-cli \
 && printf '#!/bin/sh\necho "$*" >> /tmp/notify.log\n' > /fake/notify-send \
 && chmod +x /fake/*
WORKDIR /src
COPY e2e.sh /e2e.sh
