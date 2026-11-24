#!/usr/bin/env bash
# 本のデッキを新しく作る: scripts/new-book.sh <作る場所>
set -euo pipefail
dest=${1:?usage: scripts/new-book.sh <dir>}
here=$(cd "$(dirname "$0")/.." && pwd)
if [ -e "$dest" ]; then echo "$dest は既にある" >&2; exit 1; fi
cp -r "$here/templates/book" "$dest"
cd "$dest"
git init -q
git add -A
git commit -q -m "Start a book deck from the fuzukue template"
echo "作った: $dest"
echo "原著の PDF を $dest/books/ に置いて、fuzukue を起動する: $here/scripts/run.sh $dest"
