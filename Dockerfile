FROM rust:1.85-slim AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --locked

# Real userland required: the tool shells out to sh/ls/find/grep/kill.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates coreutils findutils grep \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/mcp-ssh /usr/bin/mcp-ssh
EXPOSE 1337
ENTRYPOINT ["/usr/bin/mcp-ssh"]
CMD ["serve"]
