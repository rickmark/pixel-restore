#!/usr/bin/env bash
# Apply the read-enabled fastbootd patches to a platform tree. Idempotent:
# a patch that is already applied is detected and skipped, so re-runs are clean.
#   apply-patches.sh <AOSP_ROOT> <THIS_REPO_ROOT>
set -euo pipefail
AOSP_ROOT="${1:?usage: apply-patches.sh <AOSP_ROOT> <REPO_ROOT>}"
REPO_ROOT="${2:?usage: apply-patches.sh <AOSP_ROOT> <REPO_ROOT>}"
cd "$AOSP_ROOT"
for p in \
  "$REPO_ROOT/patches/0001-system-core-fastbootd-runtime-fetch-policy.patch" \
  "$REPO_ROOT/patches/0002-bootable-recovery-fastbootd-fetch-menu.patch"; do
  name="$(basename "$p")"
  if patch -p1 --dry-run --silent --forward < "$p" >/dev/null 2>&1; then
    echo ">> applying $name"
    patch -p1 --forward < "$p"
  elif patch -p1 --dry-run --silent --reverse < "$p" >/dev/null 2>&1; then
    echo ">> $name already applied; skipping"
  else
    echo "!! $name does not apply cleanly here."
    echo "   Your tree likely differs from the base (system/core @ AOSP main;"
    echo "   bootable/recovery @ GrapheneOS 17). Review and apply by hand."
    exit 1
  fi
done
echo ">> patches in place."
