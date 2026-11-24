#!/bin/sh
# Linux and macOS share the server installer's release selection and verification.
set -eu
dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
exec sh "$dir/install-server.sh" --client "$@"
