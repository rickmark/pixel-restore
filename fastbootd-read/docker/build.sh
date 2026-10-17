#!/usr/bin/env bash
# One command, from a Mac, to produce a read-enabled fastbootd/recovery for
# komodo. It builds the Linux container, then runs the full pipeline inside it:
#   repo init -> repo sync -> branch -> apply patches -> build -> verify
#
#   ./docker/build.sh
#
# macOS cannot build AOSP directly, so everything runs in the container. The
# platform tree is kept in a host directory (TREE) and reused across runs, so
# the huge `repo sync` only happens once.
#
# Variables (all optional):
#   TREE            Host dir for the platform tree (default: ./.aosp-tree next to
#                   this repo). Hundreds of GB once synced; put it on a fast,
#                   roomy disk. Reused across runs.
#   MANIFEST_URL    repo manifest (default: GrapheneOS platform_manifest).
#   MANIFEST_BRANCH Manifest branch/tag (default: 17). Set to the current komodo
#                   branch from grapheneos.org/build or the manifest repo.
#   LUNCH_TARGET    lunch combo (default: komodo-userdebug).
#   JOBS            parallelism (default: container nproc).
#   SKIP_SYNC       1 to skip repo init/sync (tree already current).
#   CCACHE_DIR      Host ccache dir (default: $TREE/.ccache).
#   PLATFORM        Container platform (default: linux/amd64 for the x86_64 AOSP
#                   host toolchain; override only on a native x86_64 host).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/.." && pwd)"
TREE="${TREE:-$repo_root/.aosp-tree}"
CCACHE_DIR="${CCACHE_DIR:-$TREE/.ccache}"
PLATFORM="${PLATFORM:-linux/amd64}"
mkdir -p "$TREE" "$CCACHE_DIR"

echo ">> Building Docker image (fastbootd-read-builder)"
docker build --platform "$PLATFORM" -t fastbootd-read-builder "$here"

echo ">> Running the full build pipeline in the container"
echo "   tree:   $TREE"
echo "   target: ${LUNCH_TARGET:-komodo-userdebug}"
docker run --rm -it --platform "$PLATFORM" \
  -v "$TREE":/aosp \
  -v "$repo_root":/patches:ro \
  -v "$CCACHE_DIR":/ccache \
  -e CCACHE_DIR=/ccache -e USE_CCACHE=1 \
  -e AOSP_DIR=/aosp \
  -e REPO_ROOT=/patches \
  -e MANIFEST_URL="${MANIFEST_URL:-}" \
  -e MANIFEST_BRANCH="${MANIFEST_BRANCH:-}" \
  -e LUNCH_TARGET="${LUNCH_TARGET:-}" \
  -e JOBS="${JOBS:-}" \
  -e SKIP_SYNC="${SKIP_SYNC:-0}" \
  fastbootd-read-builder \
  bash -lc 'ccache -M 50G >/dev/null 2>&1 || true; exec /patches/scripts/sync-and-build.sh'

echo ">> Done. Image artifacts are under $TREE/out/target/product/komodo"
