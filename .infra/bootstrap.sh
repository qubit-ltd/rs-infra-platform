#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
project_root=${RS_INFRA_PROJECT_ROOT:-$project_root}
[[ $# -ge 1 ]] || { echo "usage: bootstrap.sh <upstream-script> [ARGS...]" >&2; exit 2; }
entrypoint=$1
shift
case "$entrypoint" in
    .infra/bin/*.sh) ;;
    *) echo "error: unsupported infrastructure entrypoint '$entrypoint'" >&2; exit 2 ;;
esac

if [[ -n "${RS_INFRA_CACHE_DIR:-}" ]]; then
    cache_home=$RS_INFRA_CACHE_DIR
elif [[ "${RUNNER_OS:-}" == "Windows" && -n "${RUNNER_TEMP:-}" ]]; then
    cache_home="$RUNNER_TEMP/qubit"
else
    cache_home=${XDG_CACHE_HOME:-${HOME:?HOME is not set}/.cache}/qubit
fi
cache_root="$cache_home/rs-infra"
mkdir -p "$cache_root/sources" "$cache_root/tools" "$cache_root/locks" "$cache_root/build/manager"
manager_repo=https://github.com/qubit-ltd/rs-infra-tools.git

if [[ -n "${RS_INFRA_SHARED_ROOT:-}" && -x "${RS_INFRA_TOOLS_BIN:-}" ]]; then
    shared_root=$RS_INFRA_SHARED_ROOT
    manager_bin=$RS_INFRA_TOOLS_BIN
else
    infra_retry() {
        local description="$1" attempt=1 max_attempts="${RS_INFRA_NETWORK_MAX_ATTEMPTS:-6}" delay="${RS_INFRA_NETWORK_RETRY_DELAY_SECONDS:-2}" status=0
        shift
        [[ "$max_attempts" =~ ^[0-9]+$ ]] && (( max_attempts > 0 )) || { echo "error: invalid retry count" >&2; return 2; }
        [[ "$delay" =~ ^[0-9]+$ ]] || { echo "error: invalid retry delay" >&2; return 2; }
        while (( attempt <= max_attempts )); do
            if "$@"; then return 0; else status=$?; fi
            if (( attempt == max_attempts )); then echo "error: $description failed after $attempt attempt(s)" >&2; return "$status"; fi
            echo "warning: $description failed (attempt $attempt/$max_attempts); retrying in ${delay}s" >&2
            sleep "$delay"
            delay=$((delay * 2)); attempt=$((attempt + 1))
        done
    }
    manager_revision=$(infra_retry "resolve rs-infra-tools main revision" git ls-remote "$manager_repo" refs/heads/main | awk 'NR == 1 { print $1 }')
    [[ "$manager_revision" =~ ^[0-9a-f]{40}$ ]] || {
        echo "error: unable to resolve rs-infra-tools main revision" >&2; exit 1;
    }
    shared_root="$cache_root/sources/rs-infra-tools/$manager_revision"
    manager_bin="$cache_root/tools/rs-infra-tools/$manager_revision/$(rustc -vV | awk '/^host:/ { print $2 }')/$(rustc --version | tr ' /' '__')/bin/rs-infra-tools"
    lock="$cache_root/locks/manager-$manager_revision.lock"
    while ! mkdir "$lock" 2>/dev/null; do
        owner=$(cat "$lock/pid" 2>/dev/null || true)
        if [[ "$owner" =~ ^[0-9]+$ ]] && ! kill -0 "$owner" 2>/dev/null; then
            rm -rf -- "$lock"
            continue
        fi
        sleep 1
    done
    printf '%s\n' "$$" > "$lock/pid"
    cleanup_lock() { rm -rf -- "$lock"; }
    trap cleanup_lock EXIT

    if [[ ! -d "$shared_root/.git" ]]; then
        mkdir -p "$(dirname "$shared_root")"
        git init "$shared_root" >/dev/null
        git -C "$shared_root" remote add origin "$manager_repo"
    fi
    if ! git -C "$shared_root" cat-file -e "$manager_revision^{commit}" 2>/dev/null; then
        infra_retry "fetch rs-infra-tools revision" git -C "$shared_root" fetch --force --depth 1 origin "$manager_revision"
    fi
    git -C "$shared_root" checkout --force --detach "$manager_revision" >/dev/null
    [[ "$(git -C "$shared_root" rev-parse HEAD)" == "$manager_revision" ]] || {
        echo "error: cached rs-infra-tools checkout does not match $manager_revision" >&2; exit 1;
    }
    if [[ ! -x "$manager_bin" ]]; then
        mkdir -p "$(dirname "$manager_bin")"
        export CARGO_TARGET_DIR="$cache_root/build/manager/$manager_revision/$(rustc -vV | awk '/^host:/ { print $2 }')/$(rustc --version | tr ' /' '__')"
        infra_retry "fetch rs-infra-tools dependencies" cargo fetch --locked --manifest-path "$shared_root/Cargo.toml"
        cargo build --release --locked --offline --manifest-path "$shared_root/Cargo.toml" \
            --package qubit-infra-tools --bin rs-infra-tools
        mkdir -p "$(dirname "$manager_bin")"
        cp "$CARGO_TARGET_DIR/release/rs-infra-tools" "$manager_bin.tmp"
        mv "$manager_bin.tmp" "$manager_bin"
    fi
    cleanup_lock
    trap - EXIT
fi

export RS_INFRA_PROJECT_ROOT="$project_root"
export RS_INFRA_SHARED_ROOT="$shared_root"
export RS_INFRA_TOOLS_BIN="$manager_bin"
export RS_INFRA_CACHE_DIR="$cache_home"

prepare_dependencies() {
    local helper="$shared_root/.infra/lib/prepare-local-path-dependencies.sh"
    if [[ -x "$helper" ]]; then
        "$helper"
    fi
}

run_latest() {
    local tool="$1"
    shift
    "$manager_bin" latest --project "$project_root" --tool "$tool" -- "$@"
}

case "$entrypoint" in
    .infra/bin/align-ci.sh)
        prepare_dependencies
        run_latest rs-infra-style --project "$project_root" fix "$@"
        alignment_preview=false
        for arg in "$@"; do
            case "$arg" in --dry-run|-h|--help) alignment_preview=true ;; esac
        done
        if [[ "$alignment_preview" == false ]]; then
            run_latest rs-infra-ci --project "$project_root" --only clippy check
        fi
        ;;
    .infra/bin/ci-check.sh)
        source "$shared_root/.infra/lib/cleanup-build-artifacts.sh"
        prepare_dependencies
        run_latest rs-infra-ci --project "$project_root" "$@" check
        ;;
    .infra/bin/coverage.sh)
        prepare_dependencies
        run_latest rs-infra-coverage --project "$project_root" collect "$@"
        "$shared_root/.infra/lib/coverage-report.sh"
        ;;
    .infra/bin/dependency-update.sh)
        prepare_dependencies
        mode=update
        for arg in "$@"; do
            case "$arg" in
                --check) mode=check ;;
                --update) mode=update ;;
                -h|--help) echo "Usage: ./.infra/bin/dependency-update.sh [--check|--update]"; exit 0 ;;
                *) echo "error: unknown dependency-update option '$arg'" >&2; exit 2 ;;
            esac
        done
        if [[ "$mode" == update ]]; then
            "$manager_bin" latest --project "$project_root" --tool rs-infra-dependency -- --project "$project_root" --recursive sync
        fi
        run_latest rs-infra-dependency --project "$project_root" --recursive check
        ;;
    .infra/bin/infra-tool.sh)
        exec "$shared_root/.infra/lib/infra-tool.sh" "$@"
        ;;
    .infra/bin/prepare-local-path-dependencies.sh)
        exec "$shared_root/.infra/lib/prepare-local-path-dependencies.sh" "$@"
        ;;
    .infra/bin/project-ci-check.sh)
        project_hook="$project_root/.infra/project-ci-check.local.sh"
        if [[ -x "$project_hook" ]]; then
            exec "$project_hook" "$@"
        fi
        exit 0
        ;;
    .infra/bin/style-check.sh)
        prepare_dependencies
        run_latest rs-infra-style --project "$project_root" check "$@"
        ;;
    *)
        echo "error: unsupported infrastructure entrypoint '$entrypoint'" >&2
        exit 2
        ;;
esac
