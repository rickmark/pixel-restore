#!/usr/bin/env python3
"""Make a deliberately broken copy of an FBPK v2 `bootloader-*.img`, for the
brick-and-recover test in tools/brick_test.sh.

One partition's *body* is corrupted (the 4096-byte signed header is left
intact, so the image still looks like the right version to the flasher) and
the entry's CRC32 is recomputed so `fastboot flash bootloader` accepts the
container. The stage that loads the partition then fails its hash check and
the phone drops into USB boot mode, which is what pixel-restore recovers.

Usage:
    python3 tools/tamper_fbpk.py --image bootloader-husky-xxx.img \
        --partition abl --out bootloader-husky-xxx.TAMPERED.img
    python3 tools/tamper_fbpk.py --image X --partition bl1 --wipe --out Y

--wipe zeroes the whole body; the default flips 4 KiB at its start, which is
enough for any hash check and keeps the diff small.
"""
import argparse
import struct
import sys
import zlib

FBPK_MAGIC = 0x4B504246
HEADER = 4096
ENTRY_FMT = "<I36s40sQQII"


def entries(data):
    magic, version, hsize, esize = struct.unpack_from("<IIII", data, 0)
    if magic != FBPK_MAGIC or version != 2:
        sys.exit("not an FBPK v2 image")
    total = struct.unpack_from("<I", data, 104)[0]
    for i in range(total):
        base = hsize + i * esize
        kind, name, product, off, size, slotted, crc = struct.unpack_from(
            ENTRY_FMT, data, base
        )
        if kind == 0:
            continue
        yield base, name.split(b"\0")[0].decode(), off, size, crc


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--image", required=True, help="bootloader-*.img (FBPK v2)")
    ap.add_argument("--partition", default="abl",
                    help="entry to corrupt, e.g. abl, bl1, bl2, tzsw (default abl)")
    ap.add_argument("--out", required=True, help="where to write the tampered copy")
    ap.add_argument("--wipe", action="store_true", help="zero the whole body")
    ap.add_argument("--bytes", type=int, default=4096,
                    help="how many body bytes to flip without --wipe (default 4096)")
    args = ap.parse_args()

    data = bytearray(open(args.image, "rb").read())
    want = args.partition.lower()
    hit = None
    for base, name, off, size, crc in entries(data):
        bare = name[:-2] if name.endswith(("_a", "_b")) else name
        if name.lower() == want or bare.lower() == want:
            hit = (base, name, off, size, crc)
            break
    if hit is None:
        names = ", ".join(n for _, n, *_ in entries(data))
        sys.exit(f"no partition '{args.partition}' in {args.image} (have: {names})")

    base, name, off, size, crc = hit
    body = data[off:off + size]
    if zlib.crc32(bytes(body)) != crc:
        sys.exit(f"{name}: CRC already wrong in the source image; refusing to use it")
    if size <= HEADER:
        sys.exit(f"{name} has no body to corrupt ({size} bytes)")

    body_start = off + HEADER
    if args.wipe:
        data[body_start:off + size] = bytes(size - HEADER)
        how = f"zeroed {size - HEADER} body bytes"
    else:
        n = min(args.bytes, size - HEADER)
        for i in range(body_start, body_start + n):
            data[i] ^= 0xFF
        how = f"flipped {n} bytes at body offset 0"

    new_crc = zlib.crc32(bytes(data[off:off + size]))
    struct.pack_into("<I", data, base + struct.calcsize("<I36s40sQQII") - 4, new_crc)
    open(args.out, "wb").write(data)
    print(f"{name}: {how}; entry CRC {crc:08x} -> {new_crc:08x}; wrote {args.out}")


if __name__ == "__main__":
    main()
