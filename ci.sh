#!/usr/bin/env bash
# CI quality gate for pipewire-vircam.
# Usage: ./ci.sh
set -euo pipefail
cd "$(dirname "$0")"

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo package --allow-dirty
cargo test --benches

# Cognitive complexity threshold (arborist).
# Most complex function: run() = 17.
arborist src/ --threshold 20 --exceeds-only

echo "✓ All checks passed."
