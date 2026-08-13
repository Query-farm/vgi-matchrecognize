# Copyright 2026 Query Farm LLC - https://query.farm
#
# Single image that serves every network transport of the `matchrecognize` VGI
# worker:
#   docker run ... IMG            -> HTTP server on $PORT      (default; Fly.io / local)
#   docker run ... IMG tcp        -> raw Arrow-IPC over TCP on $PORT_TCP
#   docker run -i ... IMG stdio   -> stdio worker DuckDB spawns on-host
# See docker-entrypoint.sh.
#
# Unlike the stateless workers in this family, `match_recognize` is a BUFFERING
# function: it spools the whole input relation to `$TMPDIR` before it matches
# anything, and shards to disk when the relation exceeds its memory budget. Two
# consequences for anyone running this image:
#
#   - **Give it temp space.** The spool is ~24 bytes per row of the columns the
#     pattern reads, and a sharded run peaks at roughly 1.5x that. With no volume
#     mounted it lands in the container's writable layer; mount one at /tmp (or
#     point TMPDIR elsewhere) for inputs that will not fit there.
#   - **Every phase of a query must reach the same spool.** The buffering and the
#     producing phase are separate worker processes, and the spool is local to the
#     container that wrote it. Two consequences, both verified against the signed
#     vgi extension:
#       * over HTTP/TCP one container serves every phase, so it just works — but
#         behind a load balancer it needs sticky routing or a single replica;
#       * over **stdio the extension spawns a pool of workers, so each spawn is a
#         separate container** and they share nothing. Mount one volume on /tmp
#         across them (`docker run -i --rm -v mr-spool:/tmp IMG stdio`) or the
#         phases cannot find each other.
#     Getting it wrong fails loudly — the sink-count guard raises — rather than
#     returning a short answer.
#
# There is no attach-scoped state beyond that spool: it is deleted when finalize
# has read it, and orphans age out on a TTL sweep. So still no /data volume and
# no `farm.query.vgi.volumes` mount-discovery label.
# syntax=docker/dockerfile:1

# ---- build stage -----------------------------------------------------------
# Pinned glibc (bookworm) so the binary links against the same libc the slim
# runtime ships. The base rust image already carries the C toolchain that
# `libsqlite3-sys` (bundled SQLite, the SDK's default store) and `zstd-sys` need;
# nothing else in the tree builds native code, so there is no cmake step here.
FROM rust:1-bookworm AS build
WORKDIR /src

# Copy the whole workspace (manifests + sources + lockfile). The cargo registry
# and target dir are BuildKit cache mounts, so the binary is copied OUT to a
# non-cache path before the layer ends (cache mounts don't persist in the image).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin vgi-matchrecognize-worker \
    && cp target/release/vgi-matchrecognize-worker /vgi-matchrecognize-worker

# ---- runtime stage ---------------------------------------------------------
# debian-slim (not distroless) so the HEALTHCHECK below has a real `curl`. The
# HTTP transport is plain inbound HTTP (no TLS in the dependency tree), so no
# libssl / ca-certificates are needed for the worker to serve.
FROM debian:bookworm-slim

# Build metadata, wired from docker/metadata-action outputs in CI.
ARG VERSION=0.0.0
ARG GIT_COMMIT=unknown
ARG SOURCE_URL=https://github.com/Query-farm/vgi-matchrecognize

# Standard OCI labels + the VGI transport-advertisement label. `transports` lists
# the NETWORK transports this image serves (http + raw tcp); stdio is a spawn
# mode, not a network transport, so it is not listed.
LABEL org.opencontainers.image.title="vgi-matchrecognize" \
      org.opencontainers.image.description="SQL:2016 MATCH_RECOGNIZE row pattern matching as a VGI worker for DuckDB/SQL (stdio + HTTP + TCP)" \
      org.opencontainers.image.source="${SOURCE_URL}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${GIT_COMMIT}" \
      org.opencontainers.image.licenses="MIT" \
      farm.query.vgi.transports='["http","tcp"]'

ENV PORT=8000 \
    PORT_TCP=8001 \
    # Match the ATTACH name the extension uses (`ATTACH 'mr' …`); main.rs
    # defaults this to `mr`, set here explicitly for clarity.
    VGI_WORKER_CATALOG_NAME=mr \
    # Build provenance only; the version the worker advertises over VGI comes
    # from the compiled CARGO_PKG_VERSION, not this.
    VGI_MATCHRECOGNIZE_GIT_COMMIT=${GIT_COMMIT}

WORKDIR /app

# curl backs the HEALTHCHECK below; nothing else is needed at runtime.
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*

# `--chmod` sets the mode in the COPY layer itself. A separate `RUN chmod` would
# rewrite the whole binary into a second layer (overlayfs copies the file on a
# metadata change), needlessly doubling its on-disk footprint in the image.
COPY --from=build --chmod=0755 /vgi-matchrecognize-worker /usr/local/bin/vgi-matchrecognize-worker
COPY --chmod=0755 docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

# Run unprivileged. The spool lives under $TMPDIR (/tmp, mode 1777) in a
# per-uid directory the worker creates 0700, so an unprivileged user needs
# nothing granted to it.
RUN useradd --create-home --uid 10001 app
USER app

EXPOSE 8000 8001

# Readiness probe for HTTP mode. Inert for a short-lived stdio container, which
# has no HTTP server (the probe just fails harmlessly there).
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS "http://localhost:${PORT:-8000}/health" || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["http"]
