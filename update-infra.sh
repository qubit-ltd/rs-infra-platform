#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
project_root=$script_dir
repository=https://github.com/qubit-ltd/rs-infra-tools.git
mode=update

if [[ $# -gt 1 ]]; then
    echo "usage: ./update-infra.sh [--dry-run|--status|--check]" >&2
    exit 2
fi
if [[ $# -eq 1 ]]; then
    case "$1" in
        --dry-run) mode=dry-run ;;
        --status) mode=status ;;
        --check) mode=check ;;
        --yes) mode=yes ;;
        -h|--help)
            echo "Usage: ./update-infra.sh [--dry-run|--status|--check]"
            echo "Updates without prompting by default. --yes is accepted for compatibility."
            exit 0
            ;;
        *) echo "error: unknown update option '$1'" >&2; exit 2 ;;
    esac
fi

python_cmd=${RS_INFRA_PYTHON:-}
if [[ -z "$python_cmd" ]]; then
    if command -v python3 >/dev/null 2>&1; then
        python_cmd=python3
    elif command -v python >/dev/null 2>&1; then
        python_cmd=python
    else
        echo "error: Python 3 is required to manage infrastructure files" >&2
        exit 2
    fi
fi
command -v "$python_cmd" >/dev/null 2>&1 || {
    echo "error: Python executable '$python_cmd' was not found" >&2
    exit 2
}
if ! "$python_cmd" -c 'import sys; raise SystemExit(sys.version_info < (3, 10))'; then
    echo "error: Python 3.10 or later is required to manage infrastructure files" >&2
    exit 2
fi

configs_only=false
if [[ -d "$project_root/.infra" && -f "$project_root/Cargo.toml" ]] && awk '
    $0 == "[package]" { package_section = 1; next }
    /^\[/ { package_section = 0 }
    package_section && $0 ~ /^[[:space:]]*name[[:space:]]*=[[:space:]]*"qubit-infra-tools"[[:space:]]*$/ { found = 1 }
    END { exit !found }
' "$project_root/Cargo.toml"; then
    configs_only=true
fi

if [[ "$mode" == check ]]; then
    if [[ ! -d "$project_root/.infra" ]]; then
        echo "error: infrastructure is not initialized; run ./update-infra.sh first" >&2
        exit 1
    fi
    checker="$project_root/.infra/bootstrap-check.py"
    [[ -f "$checker" ]] || { echo "error: bootstrap checker is unavailable: $checker" >&2; exit 1; }
    exec "$python_cmd" "$checker"
fi

if [[ -d "$project_root/.infra" ]]; then
    bootstrap="$project_root/.infra/bootstrap.sh"
    [[ -x "$bootstrap" ]] || {
        echo "error: installed infrastructure bootstrap is unavailable: $bootstrap" >&2
        exit 1
    }
    update_args=()
    if [[ "$configs_only" == true ]]; then
        update_args+=(--configs-only)
    fi
    case "$mode" in
        dry-run) update_args+=(--dry-run) ;;
        status) update_args+=(--status) ;;
        yes) update_args+=(--yes) ;;
    esac
    exec "$bootstrap" .infra/bin/bootstrap-update.sh "${update_args[@]}"
fi

command -v git >/dev/null 2>&1 || {
    echo "error: Git is required to initialize infrastructure" >&2
    exit 2
}

tmp_root=${TMPDIR:-/tmp}
work=$(mktemp -d "$tmp_root/rs-infra-bootstrap.XXXXXX")
printf '%s\n' 'rs-infra-bootstrap-temp-v1' > "$work/.rs-infra-bootstrap-temp"
cleanup() {
    rm -rf -- "$work"
}
trap cleanup EXIT

if ! git clone --quiet --depth 1 --single-branch --branch main "$repository" "$work/source"; then
    echo "error: unable to fetch the latest rs-infra-tools bootstrap package" >&2
    exit 1
fi
export RS_INFRA_SOURCE_REVISION
RS_INFRA_SOURCE_REVISION=$(git -C "$work/source" rev-parse HEAD)

sync_args=(--project-root "$project_root" --package-root "$work/source/assets/project-bootstrap")
case "$mode" in
    dry-run) sync_args+=(--dry-run) ;;
    status) sync_args+=(--status) ;;
    yes) sync_args+=(--yes) ;;
esac
"$python_cmd" "$work/source/assets/project-bootstrap/sync.py" "${sync_args[@]}"

if [[ "$mode" != dry-run && "$mode" != status ]]; then
    command -v cargo >/dev/null 2>&1 || {
        echo "error: Rust and Cargo are required to prepare infrastructure tools" >&2
        exit 2
    }
    CARGO_TARGET_DIR="$work/build/manager" cargo run --locked \
        --manifest-path "$work/source/Cargo.toml" \
        --package qubit-infra-tools --bin rs-infra-tools -- \
        prewarm --project "$project_root"
fi
