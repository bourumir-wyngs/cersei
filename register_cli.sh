#!/usr/bin/env bash
set -e
cd "$(dirname "$0")"

cargo install --path crates/abstract-cli --force 2>&1 | tail -3
echo ""
abstract --version
