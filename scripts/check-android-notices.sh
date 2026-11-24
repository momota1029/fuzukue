#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
android="$root/android"
exec "${JAVA_HOME:+$JAVA_HOME/bin/}java" \
    "$android/app/src/test/java/dev/labgui/remote/AndroidNoticeCheck.java" "$android"
