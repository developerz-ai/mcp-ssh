# syntax=docker/dockerfile:1
# Rust version pinned to match rust-toolchain.toml (bump both together).
FROM lukemathwalker/cargo-chef:0.1.78-rust-1.100.0-slim-bookworm AS chef
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
# Cook deps first: this layer is cached until Cargo.toml/Cargo.lock change,
# so editing source no longer rebuilds every dependency.
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked

# Real userland required: the tool shells out to sh/bash/ls/find/grep.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates coreutils findutils grep tini \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/mcp-ssh /usr/bin/mcp-ssh
# Never run as root: dedicated service user owning its state dir (config
# default). A real home + bash login shell so `interactive=true` has a ~/.bashrc.
RUN useradd --system --create-home --shell /bin/bash mcp-ssh \
    && mkdir -p /var/lib/mcp-ssh/logs/jobs \
    && chown -R mcp-ssh:mcp-ssh /var/lib/mcp-ssh \
    && chmod 700 /var/lib/mcp-ssh
USER mcp-ssh
# Loopback inside a container is unreachable from `-p`; bind all interfaces.
# A non-loopback bind makes MCP_SSH_ALLOWED_HOSTS mandatory (startup fails
# without it) — pass it with `-e`.
ENV MCP_SSH_BIND=0.0.0.0:1337
# OAuth tokens, registered clients and job history live here; mount a volume so
# they survive `docker rm`.
VOLUME /var/lib/mcp-ssh
EXPOSE 1337
# No curl in the image: speak just enough HTTP over bash's /dev/tcp. /healthz is
# unauthenticated and skips the Host allowlist. Follows MCP_SSH_PORT if set.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/${MCP_SSH_PORT:-1337} && printf 'GET /healthz HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q '^HTTP/1\\.[01] 200' <&3"]
# tini as PID 1 reaps orphaned job processes. Without it they stay zombies
# (mcp-ssh doesn't reap what it didn't spawn), a zombie still answers a
# process-group probe, and `job kill` reports a dead group as still running.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/bin/mcp-ssh"]
CMD ["serve"]
