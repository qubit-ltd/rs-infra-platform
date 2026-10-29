#!/usr/bin/env bash
set -euo pipefail

project_root="$(pwd -P)"
prefixes=()
dry_run=0

usage() {
    cat <<'EOF'
用法：.infra/bin/update-dependencies-by-prefix.sh [--project <crate目录>] --prefix <前缀> [--prefix <前缀> ...]

把项目目录及其子目录中名称匹配任一前缀的 crates.io 直接依赖更新到最新稳定版本对应的 `^major.minor` 要求，忽略 patch。
保留 features、optional、default-features 等其他依赖设置；Git 和 path 依赖会跳过。

选项：
  --project <目录>  Cargo crate 目录，默认当前目录
  --prefix <前缀>   crate 名称前缀，可重复指定
  --dry-run         只显示 Cargo 计划，不写入 Cargo.toml
  -h, --help        显示帮助

示例：
  .infra/bin/update-dependencies-by-prefix.sh --prefix qubit- --prefix rs-
EOF
}

add_prefix() {
    local prefix="$1"
    [[ "$prefix" =~ ^[[:alnum:]_-]+$ ]] || {
        printf '无效的 crate 名称前缀：%s\n' "$prefix" >&2
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
            printf '未知参数：%s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

((${#prefixes[@]} > 0)) || { printf '至少需要一个 --prefix。\n' >&2; usage >&2; exit 2; }
command -v cargo >/dev/null || { printf '需要 Cargo。\n' >&2; exit 2; }
command -v jq >/dev/null || { printf '需要 jq。\n' >&2; exit 2; }
command -v curl >/dev/null || { printf '需要 curl 访问 crates.io。\n' >&2; exit 2; }
cargo add --help >/dev/null 2>&1 || { printf '当前 Cargo 不支持 cargo add。\n' >&2; exit 2; }

project_root="$(cd -- "$project_root" && pwd -P)"
manifest="${project_root}/Cargo.toml"
[[ -f "$manifest" ]] || { printf '未找到 Cargo.toml：%s\n' "$manifest" >&2; exit 2; }

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
            *) printf '跳过不支持的依赖类型：%s (%s)\n' "$name" "$kind" >&2; continue ;;
        esac
        [[ -z "$target" ]] || args+=(--target "$target")
        [[ -z "$rename" ]] || args+=(--rename "$rename")
        latest_version="$(curl --fail --silent --show-error \
            --user-agent 'rs-infra-tools dependency-prefix-updater' \
            "https://crates.io/api/v1/crates/${name}" \
            | jq -er '.crate.max_stable_version // empty')" || {
            printf '无法获取 %s 在 crates.io 上的最新稳定版本。\n' "$name" >&2
            exit 1
        }
        if [[ "$latest_version" =~ ^([0-9]+)\.([0-9]+)\.[0-9]+(\+[0-9A-Za-z.-]+)?$ ]]; then
            requirement="^${BASH_REMATCH[1]}.${BASH_REMATCH[2]}"
        else
            printf 'crates.io 返回了无法识别的稳定版本：%s %s\n' "$name" "$latest_version" >&2
            exit 1
        fi
        printf '更新 %s%s：最新稳定版 %s，Cargo 要求 %s\n' \
            "$name" "${rename:+ (别名：$rename)}" "$latest_version" "$requirement"
        [[ "$dry_run" -eq 0 ]] || args+=(--dry-run)
        cargo add "${args[@]}" "${name}@${requirement}" --manifest-path "$manifest"
    done
done
if ((found == 0)); then
    printf '没有找到名称匹配前缀且来自 crates.io 的直接依赖。\n'
fi
