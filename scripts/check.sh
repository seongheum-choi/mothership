#!/usr/bin/env bash
# Everything a change must pass: formatting, clippy (pedantic, warnings are errors), tests.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets --quiet -- -D warnings
cargo test --quiet
