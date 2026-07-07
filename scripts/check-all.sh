#!/usr/bin/env bash
# Local substitute for a CI matrix: run the unit tests on the host, then
# type-check the code (including tests and examples) for the other
# supported platforms so platform-specific compile breakage is caught
# without sitting at that machine.
#
# Cross-target checks need the target's stdlib installed once:
#   rustup target add x86_64-pc-windows-msvc x86_64-unknown-linux-gnu
#
# See TESTING.md for the full pre-merge checklist.
set -euo pipefail
cd "$(dirname "$0")/.."

host_triple=$(rustc -vV | sed -n 's/^host: //p')

echo "==> cargo test ($host_triple)"
cargo test --quiet

# x86_64-unknown-linux-gnu is intentionally absent: rdev pulls in
# evdev-sys, which builds C libevdev via autotools and cannot be
# cross-checked. Re-add it when the in-tree evdev backend (pure Rust)
# replaces rdev.
targets=(
    x86_64-pc-windows-msvc
    aarch64-apple-darwin
)
installed=$(rustup target list --installed)

for target in "${targets[@]}"; do
    if [[ "$target" == "$host_triple" ]]; then
        continue
    fi
    if ! grep -qx "$target" <<<"$installed"; then
        echo "==> SKIP $target (install with: rustup target add $target)"
        continue
    fi
    echo "==> cargo check --target $target --all-targets"
    cargo check --quiet --target "$target" --all-targets
done

echo "==> all checks passed"
