#!/usr/bin/env bash
# Restore Cargo.lock from split Cargo.lock.gz.b64.part* files
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cat "$ROOT"/Cargo.lock.gz.b64.part* | tr -d '\n' | base64 -d | gzip -d > "$ROOT/Cargo.lock"
echo "Restored Cargo.lock ($(wc -c < "$ROOT/Cargo.lock") bytes)"
