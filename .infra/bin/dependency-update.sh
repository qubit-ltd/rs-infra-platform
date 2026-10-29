#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
exec "$project_root/.infra/bootstrap.sh" .infra/bin/dependency-update.sh "$@"
