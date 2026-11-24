#!/usr/bin/env bash
# サーバーとクライアントを 1 台で起動する（Mac や、デスクトップのある Linux 向け）。
#   scripts/run.sh <プロジェクト> [デッキのサブディレクトリ]
# WSL + Windows では、サーバーだけを WSL で動かし、クライアントは Windows でビルドする（README）。
set -euo pipefail
project=$(cd "${1:?usage: scripts/run.sh <project> [deck]}" && pwd)
here=$(cd "$(dirname "$0")/.." && pwd)
source "$here/scripts/compat-env.sh"
fuzukue_legacy_env FUZUKUE_DECK FUZUKUE_ADDR
export FUZUKUE_DECK=${2:-${FUZUKUE_DECK:-}}
export FUZUKUE_ADDR=${FUZUKUE_ADDR:-127.0.0.1:7878}
cargo build --release -q --manifest-path "$here/Cargo.toml" -p secretaire
cargo build --release -q --manifest-path "$here/crates/fuzukue/Cargo.toml"
"$here/target/release/secretaire" "$project" &
server=$!
trap 'kill $server 2>/dev/null; wait $server 2>/dev/null' EXIT
FUZUKUE_URL="ws://$FUZUKUE_ADDR/ws" "$here/crates/fuzukue/target/release/fuzukue"
