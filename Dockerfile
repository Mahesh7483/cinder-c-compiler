# syntax=docker/dockerfile:1
#
# Cinder playground image: the compiler, the API server and the static front end.
#
#   docker build -t cinder-playground .
#   docker run --rm -p 8080:8080 cinder-playground      # http://localhost:8080
#
# Stage 1 builds both binaries; stage 2 is a slim Debian with what the *compiler*
# needs at run time: `as` (binutils) and `cc` (gcc, used only as a linker driver),
# plus libc's static archives so user programs can be linked statically and run
# alone in an empty chroot.

FROM rust:1-slim-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN cargo build --release --locked -p cinder -p cinder-server \
 && strip target/release/cinder target/release/cinder-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc libc6-dev binutils ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/cinder /usr/local/bin/cinder
COPY --from=builder /src/target/release/cinder-server /usr/local/bin/cinder-server
COPY web /app/web

# Scratch space for compiles and runs: the server creates one directory per job here.
RUN mkdir -p /var/lib/cinder-work && chmod 755 /var/lib/cinder-work

# See crates/cinder-server/src/config.rs for every variable. PORT is set by Render.
ENV PORT=8080 \
    BIND=0.0.0.0 \
    WEB_DIR=/app/web \
    CINDER_BIN=/usr/local/bin/cinder \
    WORK_DIR=/var/lib/cinder-work \
    SANDBOX=require \
    MAX_CONCURRENT=4 \
    RUN_CPU_SECS=2 \
    RUN_WALL_SECS=5 \
    RUN_MEMORY_MB=128 \
    OUTPUT_LIMIT_BYTES=65536

# The server runs as root on purpose: for every program it chroots into an empty
# directory and drops to an unprivileged per-slot uid *in the child* before exec
# (no user namespaces are needed, which Render does not offer). User code is never
# executed as root; with SANDBOX=require the server refuses to run code if that
# cannot be guaranteed.
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s CMD ["/usr/local/bin/cinder-server", "--healthcheck"]
# tini is PID 1 so that processes orphaned by a sandboxed program are reaped: an unreaped zombie
# keeps counting against its uid's process limit and would eventually starve that sandbox slot.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/cinder-server"]
