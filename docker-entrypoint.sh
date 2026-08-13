#!/bin/sh
# Copyright 2026 Query Farm LLC - https://query.farm
#
# Dispatch the single vgi-matchrecognize image into one of its transports:
#   http   (default) the HTTP server on $PORT (8000), bound 0.0.0.0 so a
#                    published host port reaches it. Serves /health.
#   tcp              raw Arrow-IPC over TCP on $PORT_TCP (8001), bound 0.0.0.0.
#                    Used by the VGI extension's transparently-shared container.
#   stdio            a worker DuckDB spawns over stdio (on-host execution).
#                    NOTE: the extension spawns a POOL of workers, so each spawn
#                    is its own container. `match_recognize` buffers, and the
#                    buffering and producing phases must see the same spool, so
#                    this mode needs one volume shared across them:
#                      docker run -i --rm -v mr-spool:/tmp IMG stdio
#                    Without it the phases find nothing and the sink-count guard
#                    raises. HTTP and TCP are unaffected — one container serves
#                    every phase.
# Any other first argument is exec'd verbatim (escape hatch for debugging).
#
# No mode sets up state: the worker spools its buffered input under $TMPDIR by
# itself, creating and removing that directory as queries come and go.
set -e

case "${1:-http}" in
  http)
    shift 2>/dev/null || true
    # `--http` reads its bind address from VGI_HTTP_BIND (default 127.0.0.1:0,
    # an ephemeral loopback port). In a container we must bind 0.0.0.0 on a
    # FIXED port so `-p $PORT:$PORT` and the HEALTHCHECK reach it.
    export VGI_HTTP_BIND="0.0.0.0:${PORT:-8000}"
    exec vgi-matchrecognize-worker --http "$@"
    ;;
  tcp)
    shift 2>/dev/null || true
    exec vgi-matchrecognize-worker --tcp "0.0.0.0:${PORT_TCP:-8001}" "$@"
    ;;
  stdio)
    shift 2>/dev/null || true
    exec vgi-matchrecognize-worker "$@"
    ;;
  *)
    exec "$@"
    ;;
esac
