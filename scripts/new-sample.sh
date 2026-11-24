#!/usr/bin/env bash
# examples/lab を、git で管理された別のフォルダに写す。
#   scripts/new-sample.sh [写し先]   （既定は ~/labo-sample）
# Issue ごとの作業ツリー、差分、自動査読など、git が要る機能をサンプルで試すためのもの。
# エージェントに好きに書き換えさせてよい。壊れたら消して作り直す。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$HOME/labo-sample}
if [ -e "$dest" ]; then
  echo "$dest はもうある。消してから作り直すか、別の場所を指定する。" >&2
  exit 1
fi
mkdir -p "$dest"
# 組み立てた PDF や Lean のビルド結果は写さない。
tar -C "$here/examples/lab" \
  --exclude='build' --exclude='.lake' --exclude='.fuzukue' --exclude='.labo' --exclude='.corpus' \
  -cf - . | tar -C "$dest" -xf -
cat >> "$dest/.gitignore" <<'EOF'
# fuzukue の下書き・抽出結果・Lean のビルド
.fuzukue/
.labo/
.corpus/
.lake/
EOF
git -C "$dest" init -q -b main
git -C "$dest" add -A
git -C "$dest" commit -q -m "Sample lab from fuzukue examples/lab"
echo "作った: $dest"
echo "サーバー: cargo run -p secretaire -- $dest"
