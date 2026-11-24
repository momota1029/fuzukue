#!/bin/sh
# The Linux installers share the same release selection and verification.
set -eu
dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
exec sh "$dir/install-server.sh" --client "$@"
