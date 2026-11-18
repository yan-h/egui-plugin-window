#!/usr/bin/env bash
# The one gate. On macOS, the three crates' tests; elsewhere only that they
# build, since off macOS baseview and plugin-own-window are stubs that open no
# window.
set -euo pipefail
cd "$(dirname "$0")"

# egui-baseview's features are named rather than defaulted: wgpu is the one
# renderer, and `frame-log` builds only against harmonigraph's patched wgpu-hal.
egui_features=(--no-default-features --features wgpu,tracing)

# plugin-own-window has no upstream formatting to keep, so it is held to
# rustfmt and clippy; the two forks are not yet.
cargo fmt --check -p plugin-own-window
cargo clippy --locked --all-targets --no-deps -p plugin-own-window -- -D warnings

if [[ "$(uname)" == "Darwin" ]]; then
    cargo test --locked -p baseview
    cargo test --locked -p egui-baseview "${egui_features[@]}"
    cargo test --locked -p plugin-own-window
else
    cargo check --locked --all-targets -p baseview
    cargo check --locked --all-targets -p egui-baseview "${egui_features[@]}"
    cargo check --locked --all-targets -p plugin-own-window
fi
