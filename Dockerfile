# syntax=docker/dockerfile:1
FROM lukemathwalker/cargo-chef:latest-rust-1.96-slim-bookworm AS chef
WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
# Cook deps first: this layer is cached until Cargo.toml/Cargo.lock change,
# so editing source no longer rebuilds every dependency.
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked

# Real userland required: the tool shells out to sh/ls/find/grep/kill.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates coreutils findutils grep \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/mcp-ssh /usr/bin/mcp-ssh
# Never run as root: dedicated service user owning its job-log dir (config default).
RUN useradd --system --no-create-home --shell /usr/sbin/nologin mcp-ssh \
    && mkdir -p /var/lib/mcp-ssh/jobs \
    && chown -R mcp-ssh:mcp-ssh /var/lib/mcp-ssh
USER mcp-ssh
EXPOSE 1337
ENTRYPOINT ["/usr/bin/mcp-ssh"]
CMD ["serve"]
