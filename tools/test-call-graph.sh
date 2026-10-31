#!/bin/sh
# Focused #40 call-graph slice checks; ./tools/verify remains the full gate.
set -eu
case "${1:-}" in
  mcp)
    # mcp_protocol's timing and hook tests (in the mcp test binary) need process-per-test isolation, as ./tools/verify uses; plain cargo test fails the same cluster on the unmodified base.
    cargo nextest run --locked --no-fail-fast --features test-support --lib --test mcp -E 'kind(lib) | test(/^mcp_protocol::/)'
    node --test tests/mcp-catalog-sdk.test.mjs ;;
  harness)
    cargo build --locked --bin trellis
    python3 tools/verify-40-query-performance-test.py
    python3 tools/verify-40-query-performance.py --smoke --binary "${CARGO_TARGET_DIR:-target}/debug/trellis" ;;
  *) echo "usage: sh tools/test-call-graph.sh mcp|harness" >&2; exit 2 ;;
esac
