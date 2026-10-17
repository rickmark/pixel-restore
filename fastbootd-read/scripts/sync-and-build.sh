#!/usr/bin/env bash
# End-to-end build of a read-enabled fastbootd/recovery for komodo.
# Runs INSIDE the Linux build container (see docker/build.sh), or on any
# Linux host with the AOSP build deps. It does the whole pipeline:
#   repo init -> repo sync -> branch -> apply patches -> lunch -> build -> verify
#
# Re-runnable: an existing tree is updated with `repo sync` rather than
# re-initialised, and patches are re-checked before applying.
#
# Environment (all optional except where noted):
#   AOSP_DIR        Where the platform tree lives / will be synced (default: /aosp).
#   REPO_ROOT       This fastbootd-read repo root, holding patches/ and scripts/
#                   (default: the parent of this script's dir).
#   MANIFEST_URL    repo manifest to init from
#                   (default: https://github.com/GrapheneOS/platform_manifest.git).
#   MANIFEST_BRANCH Manifest branch/tag. GrapheneOS names these by platform
#                   version or build (e.g. "16", "17", or a dated tag). Default
#                   "17"; set it to the current komodo branch from the source
#                   you use (grapheneos.org/build or the manifest repo's branches).
#   LUNCH_TARGET    lunch combo (default: komodo-userdebug). userdebug matters:
#                   it defines FB_ENABLE_FETCH and keeps the recovery/fastbootd
#                   domains able to share the tmpfs policy file.
#   JOBS            parallelism for repo sync / make (default: nproc).
#   SKIP_SYNC       set to 1 to skip repo init/sync (tree already current).
#   GIT_NAME/GIT_EMAIL  identity for `repo` (defaults provided).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "$here/.." && pwd)}"
AOSP_DIR="${AOSP_DIR:-/aosp}"
MANIFEST_URL="${MANIFEST_URL:-https://github.com/GrapheneOS/platform_manifest.git}"
MANIFEST_BRANCH="${MANIFEST_BRANCH:-17}"
LUNCH_TARGET="${LUNCH_TARGET:-komodo-userdebug}"
JOBS="${JOBS:-$(nproc 2>/dev/null || echo 4)}"
SKIP_SYNC="${SKIP_SYNC:-0}"
WORK_BRANCH="fastbootd-read"

export GIT_LFS_SKIP_SMUDGE=0
git config --global user.name  "${GIT_NAME:-fastbootd-read build}" || true
git config --global user.email "${GIT_EMAIL:-fastbootd-read@localhost}" || true
git config --global color.ui false || true

mkdir -p "$AOSP_DIR"
cd "$AOSP_DIR"

if [ "$SKIP_SYNC" != "1" ]; then
  if [ ! -d .repo ]; then
    echo ">> repo init  ($MANIFEST_URL  @ $MANIFEST_BRANCH)"
    repo init -u "$MANIFEST_URL" -b "$MANIFEST_BRANCH" --depth=1 --git-lfs
  else
    echo ">> tree already initialised; updating manifest branch to $MANIFEST_BRANCH"
    repo init -b "$MANIFEST_BRANCH" || true
  fi
  echo ">> repo sync  (-j$JOBS, this is the long part)"
  repo sync -c -j"$JOBS" --fail-fast --no-clone-bundle --no-tags
else
  echo ">> SKIP_SYNC=1: using the tree already in $AOSP_DIR"
fi

# Put the two projects we patch on a work branch so the changes are tracked and
# re-runs are clean.
echo ">> repo start $WORK_BRANCH on the patched projects"
repo start "$WORK_BRANCH" system/core bootable/recovery 2>/dev/null || true

echo ">> applying patches"
"$REPO_ROOT/scripts/apply-patches.sh" "$AOSP_DIR" "$REPO_ROOT"

echo ">> building ($LUNCH_TARGET)"
set +u; source build/envsetup.sh; set -u
lunch "$LUNCH_TARGET"
m -j"$JOBS" fastbootd recovery bootimage vendorbootimage

echo ">> verifying the patched binary + locating the image"
OUT="${OUT:-$AOSP_DIR/out/target/product/komodo}" \
  "$REPO_ROOT/scripts/make-boot-image.sh" || {
    echo "!! verify/assemble step reported a problem; see output above."
    exit 1
  }

echo ">> DONE. Patched fastbootd/recovery built under $AOSP_DIR/out/target/product/komodo"
