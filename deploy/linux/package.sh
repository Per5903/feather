#!/usr/bin/env bash
# Сборка архива для Linux: dist/feather-linux-x86_64.tar.gz
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build --release -p feather-native
stage=target/package/feather
mkdir -p "$stage" dist
cp target/release/feather deploy/linux/install.sh deploy/linux/README.txt "$stage/"
cp client/desktop/icons/128x128.png "$stage/feather.png"
tar -czf dist/feather-linux-x86_64.tar.gz -C target/package feather
ls -la dist/feather-linux-x86_64.tar.gz
