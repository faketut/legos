#!/bin/bash
# prototypes/net/run.sh — build + run both UDP bench modes, save results.
set -e
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")"
rustc --edition 2021 -O udp_bench.rs -o udp_bench
OUT="results_$(date +%Y%m%d_%H%M%S).txt"
{
  echo "### $(date -u +%FT%TZ) host=$(hostname) kernel=$(uname -r) nproc=$(nproc)"
  echo "=== plain (blocking recvfrom, stock kernel path) ==="
  ./udp_bench plain 41001
  sleep 2
  echo "=== busypoll (SO_BUSY_POLL=50us + non-blocking spin) ==="
  ./udp_bench busypoll 41002
} | tee "$OUT"
echo "saved -> $OUT"
