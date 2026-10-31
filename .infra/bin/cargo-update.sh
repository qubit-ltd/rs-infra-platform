#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)

command -v cargo >/dev/null 2>&1 || {
    echo "error: Cargo is required to update project dependencies" >&2
    exit 2
}

echo "Updating Cargo dependencies in $project_root"
(cd "$project_root" && cargo update)

has_cargo_target() {
    local manifest=$1
    local crate_dir=${manifest%/Cargo.toml}
    local target_dir

    if awk '
        /^[[:space:]]*\[workspace\][[:space:]]*$/ { found = 1 }
        /^[[:space:]]*\[lib\][[:space:]]*$/ { found = 1 }
        /^[[:space:]]*\[\[(bin|example|test|bench)\]\][[:space:]]*$/ { found = 1 }
        END { exit !found }
    ' "$manifest"; then
        return 0
    fi

    for target_dir in src/bin examples tests benches; do
        if compgen -G "$crate_dir/$target_dir/*.rs" >/dev/null \
            || compgen -G "$crate_dir/$target_dir/*/main.rs" >/dev/null; then
            return 0
        fi
    done

    [[ -f "$crate_dir/src/lib.rs" || -f "$crate_dir/src/main.rs" ]]
}

while IFS= read -r -d '' manifest; do
    [[ "$manifest" == "$project_root/Cargo.toml" ]] && continue
    crate_dir=${manifest%/Cargo.toml}
    if ! has_cargo_target "$manifest"; then
        echo "Skipping Cargo manifest without targets: $manifest"
        continue
    fi
    echo "Updating Cargo dependencies in $crate_dir"
    cargo update --manifest-path "$manifest"
done < <(
    find "$project_root" \
        -type d \( \
            -name .git -o \
            -name .infra -o \
            -name target -o \
            -name build -o \
            -name dist -o \
            -name out -o \
            -name node_modules -o \
            -name artifacts -o \
            -name coverage -o \
            -name vendor \
        \) -prune -o \
        -type f -name Cargo.toml -print0
)
