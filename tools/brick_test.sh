#!/usr/bin/env bash
# Brick-and-recover test on a real phone.
#
# Flashes a bootloader.img with ONE deliberately corrupted partition into the
# current slot, reboots, waits for the phone to fall into USB boot mode,
# recovers it with `pixel-restore boot --dir <recovery pack>`, then reflashes
# the good bootloader and checks the phone comes back to fastboot on its own.
#
# Usage:
#   tools/brick_test.sh <factory-image-dir> <recovery-pack-dir> [partition]
#     factory-image-dir  the unzipped husky-xxx-factory-xxx/ folder
#     recovery-pack-dir  e.g. ~/Downloads/tensor-usbdl-v0.2.0/sources/zuma/husky
#     partition          what to corrupt; default abl (see README before using bl1)
#
# The phone must be in fastboot, unlocked, above 4200 mV. Nothing is flashed
# until you type "brick" at the prompt (or pass --yes as the 4th argument).
#
# Read the "Brick-and-recover test" section of README.md first. Corrupting
# BL1 is only recoverable while the ROM still accepts the pack's BL1; if the
# phone has taken an anti-rollback bump since the pack was signed, there is
# no way back. Start with abl.
set -euo pipefail

FACTORY=${1:?factory image dir}
PACK=${2:?recovery pack dir}
PART=${3:-abl}
YES=${4:-}
HERE=$(cd "$(dirname "$0")/.." && pwd)
PR=${PIXEL_RESTORE:-$HERE/target/release/pixel-restore}
WORK=$(mktemp -d)

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
getvar() { fastboot getvar "$1" 2>&1 | sed -n "s/^$1: *//p" | head -1; }

command -v fastboot >/dev/null || die "fastboot not in PATH"
[ -x "$PR" ] || die "pixel-restore binary not found at $PR (cargo build --release, or set PIXEL_RESTORE)"
[ -d "$PACK" ] || die "no recovery pack dir $PACK"
GOOD=$(ls "$FACTORY"/bootloader-*.img 2>/dev/null | head -1)
[ -n "$GOOD" ] || die "no bootloader-*.img in $FACTORY"

say "phone"
DEVS=$(fastboot devices | awk '{print $1}')
[ "$(printf '%s\n' "$DEVS" | grep -c .)" -eq 1 ] || die "need exactly one fastboot device, have: ${DEVS:-none}"
PRODUCT=$(getvar product)
SLOT=$(getvar current-slot)
UNLOCKED=$(getvar unlocked)
MV=$(getvar battery-voltage | tr -dc 0-9)
BLVER=$(getvar version-bootloader)
echo "product=$PRODUCT slot=$SLOT unlocked=$UNLOCKED battery=${MV}mV bootloader=$BLVER"
[ "$UNLOCKED" = "yes" ] || die "bootloader is locked; fastboot flash would be refused"
[ -n "$MV" ] && [ "$MV" -ge 4200 ] || die "battery ${MV:-?} mV; want 4200 or more"
case "$GOOD" in *"$PRODUCT"*) ;; *) die "$GOOD does not look like a $PRODUCT image";; esac

say "tampering with $PART"
BAD=$WORK/$(basename "${GOOD%.img}").TAMPERED-$PART.img
python3 "$HERE/tools/tamper_fbpk.py" --image "$GOOD" --partition "$PART" --out "$BAD"
"$PR" unpack "$BAD" | grep -E "^name|^${PART}_|^$PART " || true

say "plan"
cat <<EOF
  1. fastboot flash bootloader $(basename "$BAD")   (slot $SLOT)
  2. fastboot reboot-bootloader
  3. wait for the phone: either back in fastboot by itself (no brick, the
     other slot or a fallback took over) or a "Pixel ROM Recovery" port
  4. pixel-restore boot --dir $PACK
  5. fastboot flash bootloader $(basename "$GOOD"); fastboot reboot-bootloader
EOF
if [ "$YES" != "--yes" ]; then
  printf 'type "brick" to go ahead: '
  read -r ANSWER
  [ "$ANSWER" = "brick" ] || die "aborted, nothing flashed"
fi

say "1-2. flashing the tampered image and rebooting"
fastboot flash bootloader "$BAD"
fastboot reboot-bootloader

say "3. waiting up to 90 s for fastboot or a ROM device"
STATE=
for _ in $(seq 1 180); do
  if fastboot devices | grep -q .; then STATE=fastboot; break; fi
  if "$PR" detect >/dev/null 2>&1; then STATE=rom; break; fi
  sleep 0.5
done
case "$STATE" in
  fastboot)
    echo "the phone came back to fastboot by itself (slot=$(getvar current-slot) bootloader=$(getvar version-bootloader))."
    echo "corrupting $PART in slot $SLOT did not brick it; reflashing the good image anyway."
    ;;
  rom)
    "$PR" detect
    say "4. recovering with the pack"
    "$PR" boot --dir "$PACK" --verbose
    say "waiting up to 60 s for fastboot"
    for _ in $(seq 1 120); do fastboot devices | grep -q . && break; sleep 0.5; done
    fastboot devices | grep -q . || die "no fastboot device after the USB boot; check the phone screen and run 'pixel-restore boot --dir $PACK --wait' by hand"
    echo "RAM-booted fastboot: battery $(getvar battery-voltage), bootloader $(getvar version-bootloader)"
    ;;
  *)
    die "neither fastboot nor a ROM device showed up in 90 s. Try the button combo (Power + Vol Up + Vol Down while plugging in) and 'pixel-restore boot --dir $PACK --wait'"
    ;;
esac

say "5. restoring the good bootloader"
fastboot flash bootloader "$GOOD"
fastboot reboot-bootloader
for _ in $(seq 1 120); do fastboot devices | grep -q . && break; sleep 0.5; done
fastboot devices | grep -q . || die "phone did not come back to fastboot after reflashing; do NOT power off, run the recovery by hand"
echo "back in fastboot from flash: slot=$(getvar current-slot) bootloader=$(getvar version-bootloader)"
[ "$(getvar version-bootloader)" = "$BLVER" ] || echo "note: bootloader version changed ($BLVER -> $(getvar version-bootloader))"

case "$STATE" in
  rom) say "done: $PART in slot $SLOT bricked the phone, pixel-restore recovered it, good image restored" ;;
  *)   say "done: corrupting $PART in slot $SLOT did not brick the phone, good image restored" ;;
esac
rm -rf "$WORK"
