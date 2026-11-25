#!/usr/bin/env bash
# Usage: scripts/release.sh <version>   e.g. scripts/release.sh 0.2.0
# Bumps Cargo.toml, commits, tags vX.Y.Z and pushes, which triggers the
# GitHub release workflow.
set -euo pipefail
cd "$(dirname "$0")/.."

version=${1:-}
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]]; then
  echo "usage: $0 <version>  (e.g. 0.2.0)" >&2; exit 1
fi
tag="v$version"

branch=$(git rev-parse --abbrev-ref HEAD)
[[ $branch == main ]] || { echo "must be on main (on $branch)" >&2; exit 1; }
[[ -z $(git status --porcelain) ]] || { echo "working tree not clean" >&2; exit 1; }
git fetch origin main --tags --quiet
[[ $(git rev-parse HEAD) == $(git rev-parse origin/main) ]] || { echo "main is not in sync with origin/main" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && { echo "tag $tag already exists" >&2; exit 1; }

echo "Releasing $tag"
read -r -p "Bump Cargo.toml, commit, tag and push? [y/N] " ans
[[ $ans == y || $ans == Y ]] || { echo "aborted"; exit 1; }

# Only touch the [package] version (first match).
sed -i "0,/^version = \".*\"/s//version = \"$version\"/" Cargo.toml
cargo update --workspace --offline >/dev/null 2>&1 || cargo check --locked --offline >/dev/null 2>&1 || cargo check >/dev/null

git add Cargo.toml Cargo.lock
git commit -m "Release $tag"
git tag -a "$tag" -m "Release $tag"
git push origin main "$tag"

echo "Pushed $tag. Watch the build: https://github.com/jeromeboivin/SP404SXWaveConverter/actions"
