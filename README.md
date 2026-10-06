# pixel-restore

Boot a Tensor Pixel (Pixel 6 and newer) out of **"Pixel ROM Recovery"** mode
and into fastboot, so it can be reflashed normally.

When the boot ROM cannot find a valid bootloader in storage (a failed or
mismatched bootloader flash is the usual cause) the phone shows a black screen
and enumerates over USB as a CDC serial device called *Pixel ROM Recovery*.
Fastboot, adb and the Android Flash Tool cannot see it. The ROM will, however,
accept signed bootloader stages pushed over that serial port, one request at a
time, and once the Android bootloader (ABL) is running the phone comes up in
fastboot. This tool answers those requests directly from the `bootloader.img`
inside the factory image for your model.

It is a Rust reimplementation of the dialogue
[tensor-usbdl](https://github.com/JoshuaDoes/tensor-usbdl) reverse engineered,
with two additions: it reads the FBPK v2 `bootloader.img` (or the whole factory
ZIP) itself instead of needing pre-extracted images, and it is request driven,
so it serves whatever stage the ROM asks for rather than following a fixed
per-model script.

## Build

```sh
cargo build --release
# binary: target/release/pixel-restore
```

macOS needs no drivers or libusb; the ROM is a plain USB serial device.
On Linux, ModemManager may grab the port when it appears; stop it or add a
udev ignore rule for `18d1:4f00`. Build with `--features libudev` if you have
`libudev-dev` and want udev-based port enumeration.

## Use

1. Download the **newest factory image for the exact model** from
   <https://developers.google.com/android/images>. "bl1 header fail" or a
   NAK from the ROM almost always means the wrong model's image, or one older
   than the anti-rollback level the phone already has.
2. Unplug the phone. Hold **Power + Volume Up + Volume Down**, plug in USB and
   keep holding for about 15 seconds. `pixel-restore detect` should print the
   serial port.
3. Run the boot:

   ```sh
   pixel-restore boot --image komodo-xxxx-factory-xxxx.zip --wait
   ```

   `--image` takes either the factory ZIP or the `bootloader-*.img` from
   inside it. `--wait` keeps scanning until the phone appears, so you can start
   the tool first and then do the button dance.
4. When the ROM stops asking for stages the phone is booting ABL. Make it
   permanent from the factory image folder:

   ```sh
   fastboot devices
   fastboot getvar battery-voltage       # want > 4200 mV before flashing
   fastboot flash bootloader bootloader-*.img
   fastboot reboot-bootloader
   ./flash-all.sh
   ```

Pushing stages over USB only boots the phone once; step 4 is what fixes it.

### Other commands

```sh
pixel-restore detect --all                  # list every serial port if the phone isn't found
pixel-restore unpack bootloader-*.img       # show the partitions, verify CRCs
pixel-restore unpack factory.zip -o out/    # extract them as out/<name>.img
pixel-restore boot --image X --verbose      # print every line the ROM sends
pixel-restore boot --dir out/               # serve loose images, tensor-usbdl style
pixel-restore boot --image X --stage dpm=my-dpm.img   # override one partition
pixel-restore boot --image X --map NEWSTAGE=gsa       # teach it an unfamiliar request
```

## How the protocol works

Device → host is text, one message per line:

| line | meaning |
| --- | --- |
| `exynos_usb_booting:eub:<serial>` | banner |
| `eub:req:<serial>:<STAGE>` | please send this stage |
| `C` | clear to send the stage just requested |
| `eub:ack:...` / `eub:nak:...` | accepted / refused |
| `<stage> header fail` | signature or version check failed |
| `irom_booting_failure:...` | failure trace |

Host → device uploads are DNW frames: `ESC D N W`, a little-endian u32 of the
total frame length, the payload, and a 16-bit trailer. tensor-usbdl sends
`FF FF` as the trailer and the ROM accepts it, so that is the default here
(`--checksum sum16` sends the classic byte sum instead).

Each stage image in `bootloader.img` is a 4096-byte signed header followed by
a code body. Some requests want the whole image (`BL1`, `DPM`, `GSA1`), others
want the header and the body separately (`ABL` then `ABLB`, `BL2` then
`BL2B`, ...). The one known generational difference is `EPBL`: Pixel 6/7 want
the whole PBL, Pixel 8 and later want the header and then ask for `EPBB`.
The tool guesses from the serial the ROM reports (`9845` Pixel 6, `9855`
Pixel 7, `9865` Pixel 8; anything newer is treated like Pixel 8) and
`--epbl full|split` overrides the guess.

If no `dpm` partition exists, 4096 zero bytes are sent, which is what
tensor-usbdl does and what Pixel 7/8 ROMs accept.

## Status

* Protocol and container parsing are unit tested, and the full boot loop is
  exercised end to end against `tools/fake_rom.py`, which plays a Pixel 8
  style ROM on a pseudo-terminal and byte-checks every upload.
* **Not yet verified on real hardware.** The stage table comes from Pixel 7/8
  observations; a Pixel 9 (zumapro) ROM is assumed to behave like Pixel 8.
  Run with `--verbose` on first use. If it asks for a stage the tool does not
  know, the error names it and `--map` lets you serve it without a rebuild.

### Testing without a phone

```sh
python3 tools/fake_rom.py --image bootloader.img     # prints /dev/pts/N (Linux) or /dev/ttysNNN (macOS)
pixel-restore boot --image bootloader.img --port /dev/pts/N --verbose
```

## Credits

Protocol details come from JoshuaDoes' tensor-usbdl (AGPL-3.0; this is an
independent reimplementation, not a port of its code). The FBPK layout follows
Google's `fbpack.py` / `fbpacktool.py` published on source.android.com.
