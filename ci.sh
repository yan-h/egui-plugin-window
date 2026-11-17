#!/usr/bin/env bash
# The one gate. On macOS, both crates' tests; elsewhere only that they build,
# since off macOS baseview is a stub that opens no window.
set -euo pipefail
cd "$(dirname "$0")"

# egui-baseview's features are named rather than defaulted: wgpu is the one
# renderer, and `frame-log` builds only against harmonigraph's patched wgpu-hal.
egui_features=(--no-default-features --features wgpu,tracing)

if [[ "$(uname)" == "Darwin" ]]; then
    cargo test --locked -p baseview
    cargo test --locked -p egui-baseview "${egui_features[@]}"
else
    cargo check --locked --all-targets -p baseview
    cargo check --locked --all-targets -p egui-baseview "${egui_features[@]}"
fi
