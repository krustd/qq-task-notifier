#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/publish-arm64.sh <image> <tag> [<tag>...]

Build the current checkout for linux/arm64, push architecture-specific tags, then
assemble each multi-architecture manifest with the AMD64 tag already published by
GitHub Actions. Log in to the image registry before running this script.
EOF
}

if (($# < 2)); then
  usage >&2
  exit 64
fi

image=$1
shift
tags=("$@")

for tag in "${tags[@]}"; do
  docker buildx imagetools inspect "${image}:${tag}-amd64" >/dev/null
done

build=(docker buildx build --platform linux/arm64 --push)
for tag in "${tags[@]}"; do
  build+=(--tag "${image}:${tag}-arm64")
done
build+=(.)
"${build[@]}"

for tag in "${tags[@]}"; do
  docker buildx imagetools create \
    --tag "${image}:${tag}" \
    "${image}:${tag}-amd64" \
    "${image}:${tag}-arm64"
done
