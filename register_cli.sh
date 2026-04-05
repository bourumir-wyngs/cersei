#!/usr/bin/env bash
set -e
cd "$(dirname "$0")"

cargo install --path . --force 2>&1 | tail -3
echo ""
cersei --version
