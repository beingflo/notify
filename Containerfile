# --- build ------------------------------------------------------------------
FROM rust:bookworm AS builder
WORKDIR /usr/src/notify

COPY Cargo.toml Cargo.lock ./
# Prime the dependency layer with a throwaway binary, so changing our own source
# recompiles only our own crate.
#
# No `--mount=type=cache` anywhere: the crate registry and `target/` live in this
# layer instead, which the next RUN inherits, and podman reuses the layer itself
# while Cargo.toml and Cargo.lock are unchanged. The cache-mount version of this
# build failed once with a truncated crate unpack (`tower-http` without its
# `src/cors/` directory) and cargo does not verify those directories, so the
# layer cache is both faster here and the one that cannot go quietly wrong.
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# The `touch` is load-bearing, exactly as in acta's Containerfile: COPY keeps
# the context's mtimes, which are older than the fingerprints the priming build
# recorded, so cargo would consider the package fresh and leave /tmp/notify as
# the throwaway binary. The size check fails the build loudly if it happens.
RUN find src -exec touch {} + \
    && cargo build --release \
    && cp target/release/notify /tmp/notify \
    && test "$(stat -c %s /tmp/notify)" -gt 200000

# --- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Deliberately no `ca-certificates` and no OpenSSL: lettre is built against
# rustls with webpki roots (see the feature list in Cargo.toml), so the trust
# anchors are compiled in and the image is the base image plus one binary.
WORKDIR /usr/src/app
COPY --from=builder /tmp/notify /usr/local/bin/notify

ENV SERVE_PORT=8080 \
    RUST_LOG=info

# Numeric on purpose: the quadlet runs with `UserNS=keep-id:uid=1000,gid=1000`,
# so this uid is the deploying user on the host. The process needs no
# capability and writes nothing to disk.
USER 1000:1000

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/notify"]
