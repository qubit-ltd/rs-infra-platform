#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
project_root=$script_dir
while [[ ! -d "$project_root/.infra" && "$project_root" != / ]]; do
    project_root=$(dirname "$project_root")
done
if [[ ! -d "$project_root/.infra" ]]; then
    echo "error: unable to locate project .infra directory" >&2
    exit 2
fi
python_cmd=${RS_INFRA_PYTHON:-}
if [[ -z "$python_cmd" ]]; then
    if command -v python3 >/dev/null 2>&1; then
        python_cmd=python3
    elif command -v python >/dev/null 2>&1; then
        python_cmd=python
    else
        echo "error: Python 3 is required to manage bootstrap snapshots" >&2
        exit 2
    fi
fi

if [[ $# -eq 0 ]]; then
    set -- --yes
fi
mode=${1:-}
if [[ "$mode" == --check ]]; then
    exec "$python_cmd" "$project_root/.infra/bootstrap-check.py"
fi
if [[ "$mode" != --yes && "$mode" != --dry-run && "$mode" != --status ]]; then
    echo "usage: ./update-infra.sh [--dry-run|--status|--check] (updates without prompting by default)" >&2
    exit 2
fi

tmp_root=${TMPDIR:-/tmp}
work=$(mktemp -d "$tmp_root/rs-infra-bootstrap.XXXXXX")
printf '%s\n' 'rs-infra-bootstrap-temp-v1' > "$work/.rs-infra-bootstrap-temp"
cleanup() { rm -rf -- "$work"; }
trap cleanup EXIT
repository=https://github.com/qubit-ltd/rs-infra-tools.git
if ! git clone --quiet --depth 1 --single-branch --branch main "$repository" "$work/source"; then
    echo "error: unable to fetch the latest rs-infra-tools bootstrap package" >&2
    exit 1
fi
export RS_INFRA_SOURCE_REVISION
RS_INFRA_SOURCE_REVISION=$(git -C "$work/source" rev-parse HEAD)
if command -v cygpath >/dev/null 2>&1; then
    export RS_INFRA_BOOTSTRAP_TEMP_ROOT=$(cygpath -aw "$work")
else
    export RS_INFRA_BOOTSTRAP_TEMP_ROOT="$work"
fi
exec "$python_cmd" "$work/source/assets/project-bootstrap/sync.py" \
    --project-root "$project_root" \
    --package-root "$work/source/assets/project-bootstrap" \
    "$@"
