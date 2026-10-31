#!/usr/bin/env bash
set -euo pipefail

project_root="$(pwd -P)"
prefixes=()
dry_run=0

usage() {
    cat <<'EOF'
Usage: .infra/bin/update-dependencies-by-prefix.sh [--project <crate-dir>] --prefix <prefix> [--prefix <prefix> ...]

Update crates.io dependencies in the project directory and its subdirectories whose names match any prefix to the latest stable version's `^major.minor` requirement, ignoring patch versions.
Preserve other dependency settings such as features, optional, and default-features; skip Git and path dependencies.

Options:
  --project <dir>    Cargo crate directory (default: current directory)
  --prefix <prefix>  Crate name prefix; may be specified multiple times
  --dry-run          Show the Cargo plan without modifying Cargo.toml
  -h, --help         Show this help

Examples:
  .infra/bin/update-dependencies-by-prefix.sh --prefix qubit- --prefix rs-
EOF
}

add_prefix() {
    local prefix="$1"
    [[ "$prefix" =~ ^[[:alnum:]_-]+$ ]] || {
        printf 'Invalid crate name prefix: %s\n' "$prefix" >&2
        exit 2
    }
    prefixes+=("$prefix")
}

while (($# > 0)); do
    case "$1" in
        --project)
            (($# >= 2)) || { usage >&2; exit 2; }
            project_root="$2"
            shift 2
            ;;
        --prefix)
            (($# >= 2)) || { usage >&2; exit 2; }
            add_prefix "$2"
            shift 2
            ;;
        --dry-run)
            dry_run=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'Unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

((${#prefixes[@]} > 0)) || { printf 'At least one --prefix is required.\n' >&2; usage >&2; exit 2; }
command -v cargo >/dev/null || { printf 'Cargo is required.\n' >&2; exit 2; }
command -v jq >/dev/null || { printf 'jq is required.\n' >&2; exit 2; }
command -v curl >/dev/null || { printf 'curl is required to access crates.io.\n' >&2; exit 2; }
cargo add --help >/dev/null 2>&1 || { printf 'This Cargo version does not support cargo add.\n' >&2; exit 2; }

project_root="$(cd -- "$project_root" && pwd -P)"
manifest="${project_root}/Cargo.toml"
[[ -f "$manifest" ]] || { printf 'Cargo.toml not found: %s\n' "$manifest" >&2; exit 2; }

prefix_json="$(printf '%s\n' "${prefixes[@]}" | jq -R . | jq -s .)"
mapfile -d '' -t manifests < <(
    find "$project_root" -mindepth 1 -type d \( -name '.*' -o -name target -o -path '*/tests/fixtures' \) -prune -o \
        -type f -name Cargo.toml -print0 | sort -z
)
found=0
for manifest in "${manifests[@]}"; do
    metadata="$(cargo metadata --manifest-path "$manifest" --format-version 1 --no-deps)"
    mapfile -t dependencies < <(
        jq -r --argjson prefixes "$prefix_json" --arg manifest "$manifest" '
        .packages[]
        | select(.manifest_path == $manifest)
        | .dependencies[]
        | select(.source == "registry+https://github.com/rust-lang/crates.io-index")
        | select(.name as $name | any($prefixes[]; . as $prefix | $name | startswith($prefix)))
        | {name, rename, kind: (.kind // "normal"), target}
        | @json
        ' <<<"$metadata"
    )
    for dependency in "${dependencies[@]}"; do
        found=1
        name="$(jq -r '.name' <<<"$dependency")"
        rename="$(jq -r '.rename // ""' <<<"$dependency")"
        kind="$(jq -r '.kind' <<<"$dependency")"
        target="$(jq -r '.target // ""' <<<"$dependency")"
        args=()
        case "$kind" in
            dev) args+=(--dev) ;;
            build) args+=(--build) ;;
            normal) ;;
            *) printf 'Skipping unsupported dependency type: %s (%s)\n' "$name" "$kind" >&2; continue ;;
        esac
        [[ -z "$target" ]] || args+=(--target "$target")
        [[ -z "$rename" ]] || args+=(--rename "$rename")
        latest_version="$(curl --fail --silent --show-error \
            --user-agent 'rs-infra-tools dependency-prefix-updater' \
            "https://crates.io/api/v1/crates/${name}" \
            | jq -er '.crate.max_stable_version // empty')" || {
            printf 'Could not fetch the latest stable version of %s from crates.io.\n' "$name" >&2
            exit 1
        }
        if [[ "$latest_version" =~ ^([0-9]+)\.([0-9]+)\.[0-9]+(\+[0-9A-Za-z.-]+)?$ ]]; then
            requirement="^${BASH_REMATCH[1]}.${BASH_REMATCH[2]}"
        else
            printf 'crates.io returned an unrecognized stable version for %s: %s\n' "$name" "$latest_version" >&2
            exit 1
        fi
        printf 'Updating %s%s: latest stable version %s, Cargo requirement %s\n' \
            "$name" "${rename:+ (alias: $rename)}" "$latest_version" "$requirement"
        [[ "$dry_run" -eq 0 ]] || args+=(--dry-run)
        cargo add "${args[@]}" "${name}@${requirement}" --manifest-path "$manifest"
    done
done
if ((found == 0)); then
    printf 'No direct crates.io dependencies matched the specified prefixes.\n'
fi
