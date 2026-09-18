#!/usr/bin/env bash
# Restore Cargo.lock from Cargo.lock.gz.b64 (md5 3ed499256566e8d590f1e3f505ce8d1e)
set -euo pipefail
base64 -d < Cargo.lock.gz.b64 | gzip -d > Cargo.lock
md5sum Cargo.lock
