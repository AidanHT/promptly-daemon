#!/usr/bin/env bash
# Build (first run only) and start the Promptly telemetry daemon in the foreground.
#
#   ./run.sh                       # watch the CURRENT directory (cd into a level first)
#   ./run.sh --workspace /path     # watch a specific level workspace
#   ./run.sh --api-port 8999       # any extra `promptlyd run` flags pass through
#
# Ctrl-C to stop. For a permanent background service use `promptlyd install`.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="$here/target/release/promptlyd"

if [ ! -x "$bin" ]; then
    echo "Building promptly + promptlyd (first run only)..."
    cargo build --release --manifest-path "$here/Cargo.toml" -p promptlyd -p promptly
fi

echo "promptlyd  ->  API http://127.0.0.1:8765   OTLP http://127.0.0.1:4318   (Ctrl-C to stop)"
case " $* " in
    *" --workspace "*) exec "$bin" run "$@" ;;
    *)                 exec "$bin" run --workspace "$(pwd)" "$@" ;;
esac
