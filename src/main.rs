mod dnw;
mod eub;
mod fbpk;
mod stages;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Boot a Tensor Pixel out of "Pixel ROM Recovery" mode into fastboot.
///
/// When a Pixel 6 or newer cannot find a valid bootloader it enumerates over
/// USB as a serial device named "Pixel ROM Recovery" and asks the host to
/// feed it bootloader stages one at a time. This tool answers those requests
/// straight from the bootloader.img inside the factory image for your model.
/// Once ABL is running the phone comes up in fastboot and you can flash the
/// bootloader permanently with `fastboot flash bootloader ...` / flash-all.
#[derive(Parser)]
#[command(version, about, long_about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Look for a Pixel in ROM-recovery mode and print its serial port.
    Detect {
        /// Also list every other serial port, to help spot the phone when
        /// its USB ids are unexpected.
        #[arg(long)]
        all: bool,
        /// Keep scanning until a device appears.
        #[arg(long, short)]
        wait: bool,
    },
    /// Show what is inside a bootloader.img (or factory ZIP), optionally
    /// extracting every partition image.
    Unpack {
        /// bootloader-*.img, or the factory image ZIP containing one.
        image: PathBuf,
        /// Write each partition to this directory as <name>.img.
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Feed bootloader stages to the phone until it reaches fastboot.
    Boot(BootArgs),
}

#[derive(Args)]
struct BootArgs {
    /// bootloader-*.img from the factory image, or the factory ZIP itself.
    /// Must match the phone's exact model and be at least as new as what it
    /// last ran.
    #[arg(long, short)]
    image: Option<PathBuf>,

    /// Directory of loose images (bl1.img, pbl.img, abl.img ...) to use
    /// instead of, or on top of, --image.
    #[arg(long, short)]
    dir: Option<PathBuf>,

    /// Serve one partition from a specific file: `--stage abl=/path/abl.img`.
    /// Repeatable. Wins over --dir and --image.
    #[arg(long, value_name = "NAME=FILE")]
    stage: Vec<String>,

    /// Tell the tool which partition an unfamiliar ROM request refers to:
    /// `--map FOO=gsa`. Repeatable.
    #[arg(long, value_name = "REQUEST=PARTITION")]
    map: Vec<String>,

    /// Serial port to use instead of auto-detecting (e.g. /dev/cu.usbmodem1234).
    #[arg(long, short)]
    port: Option<String>,

    /// Keep scanning until the phone shows up, then boot it.
    #[arg(long, short)]
    wait: bool,

    /// How the ROM wants EPBL: `split` (header now, body on EPBB; Pixel 8+)
    /// or `full` (Pixel 6/7). Default: guess from the ROM's serial.
    #[arg(long, value_parser = ["split", "full"])]
    epbl: Option<String>,

    /// Frame checksum: `ffff` (what tensor-usbdl sends and the ROM accepts),
    /// `sum16` (classic DNW byte sum), or a 4-digit hex constant.
    #[arg(long, default_value = "ffff")]
    checksum: String,

    /// Send the DNW STOP command when connecting.
    #[arg(long)]
    stop: bool,

    /// Abort if the ROM is silent for this many seconds.
    #[arg(long, default_value_t = 20)]
    idle_timeout: u64,

    /// Print every line the ROM sends.
    #[arg(long, short)]
    verbose: bool,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Detect { all, wait } => detect(all, wait),
        Cmd::Unpack { image, out } => unpack(&image, out.as_deref()),
        Cmd::Boot(args) => boot(args),
    }
}

fn detect(all: bool, wait: bool) -> Result<()> {
    let mut polls = 0u32;
    loop {
        let devices = eub::find_devices()?;
        if !devices.is_empty() {
            for d in &devices {
                println!(
                    "Pixel ROM Recovery on {}  serial={}  product={}",
                    d.port,
                    d.serial.as_deref().unwrap_or("?"),
                    d.product.as_deref().unwrap_or("?")
                );
            }
            return Ok(());
        }
        if all {
            let ports = eub::all_ports()?;
            if ports.is_empty() {
                println!("no serial ports at all");
            }
            for p in ports {
                match p.port_type {
                    serialport::SerialPortType::UsbPort(u) => println!(
                        "  {}  usb {:04x}:{:04x}  {} {}",
                        p.port_name,
                        u.vid,
                        u.pid,
                        u.manufacturer.unwrap_or_default(),
                        u.product.unwrap_or_default()
                    ),
                    other => println!("  {}  {:?}", p.port_name, other),
                }
            }
        }
        if !wait {
            bail!(
                "no Pixel in ROM-recovery mode (USB {:04x}:{:04x}) found. \
                 Unplug the phone, hold Power + Volume Up + Volume Down, plug it in \
                 and keep holding for about 15 seconds.",
                eub::ROM_VID,
                eub::ROM_PID
            );
        }
        if polls.is_multiple_of(20) {
            eprintln!("waiting for the phone to appear in ROM-recovery mode ...");
        }
        polls += 1;
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn unpack(image: &std::path::Path, out: Option<&std::path::Path>) -> Result<()> {
    let pack = fbpk::load(image)?;
    println!(
        "FBPK v{}  platform={}  version={}  slot_type={}  align={}  total={} bytes",
        pack.version,
        pack.platform,
        pack.pack_version.trim(),
        pack.slot_type,
        pack.data_align,
        pack.total_size
    );
    println!(
        "{:<20} {:<10} {:<12} {:>12} {:>12}  {:<8} crc",
        "name", "type", "product", "offset", "size", "slotted"
    );
    for e in &pack.entries {
        let crc = if e.kind == fbpk::ENTRY_PARTITION_TABLE {
            "-".to_string()
        } else if pack.verify_crc(e) {
            "ok".to_string()
        } else {
            "MISMATCH".to_string()
        };
        println!(
            "{:<20} {:<10} {:<12} {:>12} {:>12}  {:<8} {}",
            e.name,
            e.kind_name(),
            e.product,
            e.offset,
            e.size,
            if e.slotted { "yes" } else { "no" },
            crc
        );
    }
    if let Some(dir) = out {
        std::fs::create_dir_all(dir)?;
        for e in pack.data_entries() {
            let path = dir.join(format!("{}.img", e.name));
            std::fs::write(&path, pack.entry_data(e))
                .with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}

fn parse_kv(items: &[String], what: &str) -> Result<HashMap<String, String>> {
    items
        .iter()
        .map(|s| {
            let (k, v) = s
                .split_once('=')
                .ok_or_else(|| anyhow!("--{what} expects KEY=VALUE, got '{s}'"))?;
            Ok((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn parse_checksum(s: &str) -> Result<dnw::Checksum> {
    match s.to_ascii_lowercase().as_str() {
        "sum16" | "sum" => Ok(dnw::Checksum::Sum16),
        hex => {
            let hex = hex.trim_start_matches("0x");
            let v = u16::from_str_radix(hex, 16).map_err(|_| {
                anyhow!("--checksum must be sum16 or a 16-bit hex value, got '{s}'")
            })?;
            Ok(dnw::Checksum::Fixed(v))
        }
    }
}

const LOOSE_IMAGE_NAMES: &[&str] = &[
    "bl1", "pbl", "bl2", "abl", "bl31", "tzsw", "ldfw", "gsa", "gsaf", "gcf", "dpm",
];

fn boot(args: BootArgs) -> Result<()> {
    if args.image.is_none() && args.dir.is_none() && args.stage.is_empty() {
        bail!("give me the bootloader with --image <bootloader-*.img or factory.zip>");
    }

    let pack = match &args.image {
        Some(p) => Some(fbpk::load(p)?),
        None => None,
    };
    if let Some(pack) = &pack {
        println!(
            "bootloader.img: platform {} version {} ({} partitions)",
            pack.platform,
            pack.pack_version.trim(),
            pack.data_entries().count()
        );
        let bad: Vec<_> = pack
            .data_entries()
            .filter(|e| !pack.verify_crc(e))
            .map(|e| e.name.clone())
            .collect();
        if !bad.is_empty() {
            bail!(
                "CRC mismatch in {}; the download is corrupt",
                bad.join(", ")
            );
        }
        if pack.find("bl1").is_none() {
            bail!("this bootloader.img has no bl1 partition; the ROM asks for that first");
        }
    }

    let mut files = HashMap::new();
    if let Some(dir) = &args.dir {
        for name in LOOSE_IMAGE_NAMES {
            let path = dir.join(format!("{name}.img"));
            if path.is_file() {
                files.insert(name.to_string(), std::fs::read(&path)?);
                println!("loose image: {}", path.display());
            }
        }
        if files.is_empty() {
            bail!(
                "{} holds none of {}",
                dir.display(),
                LOOSE_IMAGE_NAMES.join(".img, ")
            );
        }
    }
    for (name, path) in parse_kv(&args.stage, "stage")? {
        files.insert(
            name.to_ascii_lowercase(),
            std::fs::read(&path).with_context(|| format!("reading {path}"))?,
        );
    }
    let remap = parse_kv(&args.map, "map")?
        .into_iter()
        .map(|(k, v)| (k.to_ascii_uppercase(), v.to_ascii_lowercase()))
        .collect();

    let sources = stages::Sources { pack, files, remap };
    let opts = eub::BootOptions {
        checksum: parse_checksum(&args.checksum)?,
        generation: args.epbl.as_deref().map(|e| match e {
            "full" => stages::Generation::Legacy,
            _ => stages::Generation::Split,
        }),
        send_stop: args.stop,
        verbose: args.verbose,
        idle_timeout: Duration::from_secs(args.idle_timeout),
    };

    let port_name = match args.port {
        Some(p) => p,
        None => loop {
            let devices = eub::find_devices()?;
            if let Some(d) = devices.first() {
                if devices.len() > 1 {
                    eprintln!(
                        "more than one ROM device; using {} (pick with --port)",
                        d.port
                    );
                }
                println!(
                    "found Pixel ROM Recovery on {} (serial {})",
                    d.port,
                    d.serial.as_deref().unwrap_or("?")
                );
                break d.port.clone();
            }
            if !args.wait {
                bail!(
                    "no Pixel in ROM-recovery mode found. Hold Power + Volume Up + \
                     Volume Down, plug in USB and keep holding ~15 s, or pass --wait."
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        },
    };

    let mut port = eub::open(&port_name)?;
    let report = eub::boot(port.as_mut(), &sources, &opts)?;
    drop(port);

    println!();
    if report.stages_sent.is_empty() {
        bail!("the ROM never asked for a stage");
    }
    println!(
        "sent {} stages: {}",
        report.stages_sent.len(),
        report.stages_sent.join(" ")
    );
    if report.clean_exit {
        println!(
            "The phone should now be in fastboot. Check with `fastboot devices`, then make it stick:\n\
             \n  fastboot getvar battery-voltage   # want > 4200 mV before flashing\n  \
             fastboot flash bootloader <bootloader-*.img>\n  \
             fastboot reboot-bootloader\n  \
             ./flash-all.sh                    # from the factory image"
        );
    }
    Ok(())
}
