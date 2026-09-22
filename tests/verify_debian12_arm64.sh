#!/usr/bin/env bash
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends libvpx-dev pkg-config
test "$(uname -m)" = aarch64
test "$(pkg-config --modversion vpx)" = 1.12.0
rustc --version
dpkg-query -W libvpx-dev

cargo install --path . --locked --root /tmp/visual-store-install
cargo test --locked --lib codec::vp9::tests
cargo test --locked --test temporal_codec
cargo test --locked --test pack dry_run_then_pack_is_atomic_retryable_and_readable
cargo run --locked --example vp9_roundtrip

store=$(mktemp -d)
image=tests/fixtures/v1-store/objects/sha256/fe/96/fe9614fd5f645c8fe6e4dddb9d0bf075fa7bb6305651da42bc8063c3e18e2f97.png
vstore=/tmp/visual-store-install/bin/vstore
"$vstore" --store "$store" init
for frame in 1 2 3; do
  "$vstore" --store "$store" put --file "$image" --run debian12-vp9 --stream screen
done
"$vstore" --store "$store" pack --run debian12-vp9 --codec vp9
"$vstore" --store "$store" verify
