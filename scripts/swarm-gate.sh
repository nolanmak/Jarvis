#!/bin/bash
set -euo pipefail
. "$HOME/.cargo/env"
cargo build --release -p augmentagent-cli
cargo test
npm run build
