# Import legacy environment names used by these entry points.
fuzukue_legacy_env() {
    local name old
    for name; do
        old=LABO_${name#FUZUKUE_}
        if [[ -z ${!name+x} && -n ${!old+x} ]]; then
            printf '%s is deprecated; use %s\n' "$old" "$name" >&2
            export "$name=${!old}"
        fi
    done
}
