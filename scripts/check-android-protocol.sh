#!/bin/sh
set -eu

root=$(CDPATH= cd -P "$(dirname "$0")/.." && pwd)

protocol_version() {
    awk '
        sub(/^[[:space:]]*pub[[:space:]]+const[[:space:]]+/, "") ||
        sub(/^[[:space:]]*(internal[[:space:]]+)?const[[:space:]]+val[[:space:]]+/, "") {
            sub(/[[:space:]]*\/\/.*/, "")
            sub(/[[:space:]]*:[[:space:]]*(u32|Int)[[:space:]]*=/, " = ")
            sub(/[[:space:];]+$/, "")
            if (split($0, parts, /[[:space:]]*=[[:space:]]*/) == 2)
                values[parts[1]] = parts[2]
        }
        END {
            name = "PROTOCOL_VERSION"
            while (name in values && !(name in seen)) {
                seen[name] = 1
                name = values[name]
                if (name ~ /^[0-9]+$/) {
                    print name
                    exit
                }
            }
            exit 1
        }
    ' "$@"
}

server_dir="$root/crates/fuzukue-proto/src"
android_file="$root/android/app/src/main/java/dev/labgui/remote/Protocol.kt"
server=$(protocol_version "$server_dir"/*.rs) || { printf 'Cannot resolve PROTOCOL_VERSION in %s/*.rs\n' "$server_dir" >&2; exit 1; }
android=$(protocol_version "$android_file") || { printf 'Cannot resolve PROTOCOL_VERSION in %s\n' "$android_file" >&2; exit 1; }

if [ "$android" -lt "$server" ]; then
    printf 'Android protocol %s is lower than server protocol %s. Update PROTOCOL_VERSION in Protocol.kt.\n' "$android" "$server" >&2
    exit 1
fi
