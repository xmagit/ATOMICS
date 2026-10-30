#!/bin/bash
# Host-side Docker runner. Heavy commands belong inside `imacs queue` (see README).
set -euo pipefail

usage() {
    echo 'Usage: scripts/linux-qa.sh test | screenshot <out.png> [--size WxH] | shell' >&2
    exit 2
}

root=$(cd "$(dirname "$0")/.." && pwd)
command=${1:-}
[[ $# -gt 0 ]] || usage
shift
size=1600x1000
case "$command" in
    test|shell) [[ $# -eq 0 ]] || usage ;;
    screenshot)
        [[ $# -ge 1 && -n $1 ]] || usage
        output=$1
        shift
        if [[ -d $output ]]; then
            echo 'Screenshot output must be a file, not a directory.' >&2
            exit 2
        fi
        if [[ $# -gt 0 ]]; then
            [[ $# -eq 2 && $1 == --size ]] || usage
            size=$2
        fi
        [[ $size =~ ^([0-9]{3,4})x([0-9]{3,4})$ ]] || usage
        width=$((10#${BASH_REMATCH[1]}))
        height=$((10#${BASH_REMATCH[2]}))
        if (( width < 480 || height < 320 || width > 8192 || height > 8192 )); then
            echo 'Window size must be between 480x320 and 8192x8192.' >&2
            exit 2
        fi
        size=${width}x${height}
        ;;
    *) usage ;;
esac

if ! docker info >/dev/null; then
    echo 'Docker Desktop is unavailable. Start it and retry (sandboxed agents need Docker socket access).' >&2
    exit 1
fi

# Only our labelled target caches are eligible. Include stopped containers in the
# usage check, and let non-forced removal protect against a container starting
# between the check and removal. Existing unlabelled caches stay unlabelled.
checkout_label=com.imacs.linux-qa.checkout
volumes=$(docker volume ls --filter "label=$checkout_label" --format '{{.Name}}')
while IFS= read -r volume; do
    [[ $volume == imacs-linux-qa-target-* ]] || continue
    checkout=$(docker volume inspect "$volume" --format "{{ index .Labels \"$checkout_label\" }}" 2>/dev/null) || continue
    [[ $checkout == /* ]] || continue
    [[ ! -e $checkout && ! -L $checkout ]] || continue
    containers=$(docker ps -aq --filter "volume=$volume") || continue
    [[ -z $containers ]] || continue
    if docker volume rm "$volume" >/dev/null 2>&1; then
        echo "Removed orphaned Linux QA target cache: $volume ($checkout)" >&2
    else
        echo "Kept Linux QA target cache $volume: removal failed or it is now in use." >&2
    fi
done <<< "$volumes"

checkout_key=$(printf '%s' "$root" | cksum | awk '{print $1}')
target_volume="imacs-linux-qa-target-$checkout_key"
if ! docker volume inspect "$target_volume" >/dev/null 2>&1; then
    docker volume create --label "$checkout_label=$root" "$target_volume" >/dev/null
fi

# No source is sent to the builder. A changed Dockerfile selects a fresh image;
# unchanged runs reuse the image without resolving its base tag or running apt.
image_key=$(cksum < "$root/Dockerfile.linux-qa" | awk '{print $1}')
image="imacs-linux-qa:$image_key"
if ! docker image inspect "$image" >/dev/null 2>&1; then
    docker build -t "$image" - < "$root/Dockerfile.linux-qa"
fi

# Targets are per checkout; the registry and git downloads are shared. Host
# jobserver FIFOs cannot cross Docker Desktop's VM boundary, so cap jobs/CPUs.
run=(docker run --rm --init --cap-drop ALL
    --cpus "${IMACS_LINUX_QA_CPUS:-2}" --memory "${IMACS_LINUX_QA_MEMORY:-8g}"
    --mount "type=bind,source=$root,target=/repo,readonly"
    --mount "type=volume,source=$target_volume,target=/target"
    --mount type=volume,source=imacs-linux-qa-registry,target=/usr/local/cargo/registry
    --mount type=volume,source=imacs-linux-qa-git,target=/usr/local/cargo/git
    --env "CARGO_BUILD_JOBS=${IMACS_LINUX_QA_JOBS:-2}")

# Linked worktrees have an absolute .git pointer outside the checkout. Supply
# that metadata read-only too, so build.rs and cargo/git inspection still work.
git_common=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
if [[ $git_common != "$root/.git" ]]; then
    run+=(--mount "type=bind,source=$git_common,target=$git_common,readonly")
fi

case "$command" in
    test)
        "${run[@]}" "$image" bash /repo/scripts/linux-qa-container.sh test
        ;;
    screenshot)
        # Copy a single PNG through stdout, rather than giving the container a
        # writable host directory (which could also expose the source checkout).
        mkdir -p "$(dirname "$output")"
        temporary=$(mktemp "${output}.XXXXXX")
        trap 'rm -f "$temporary"' EXIT
        "${run[@]}" "$image" bash /repo/scripts/linux-qa-container.sh screenshot "$size" > "$temporary"
        mv -f "$temporary" "$output"
        echo "Saved Linux screenshot: $output"
        ;;
    shell)
        if [[ -t 0 && -t 1 ]]; then
            run+=(-it)
        else
            run+=(-i)
        fi
        "${run[@]}" "$image" bash
        ;;
esac
