//! Talking to a Pixel in ROM-recovery mode over its serial port: finding the
//! port, and running the request/upload loop until the ROM stops asking for
//! stages (which is when ABL takes over and the phone shows fastboot).

use crate::dnw::{self, Checksum, LineReader, Message};
use crate::stages::{self, Generation, Sources};
use anyhow::{bail, Context, Result};
use serialport::{SerialPort, SerialPortInfo, SerialPortType};
use std::io::Write;
use std::time::{Duration, Instant};

/// Google's USB vendor id and the product id the Tensor boot ROM enumerates
/// with. macOS shows it as "Pixel ROM Recovery" in System Information.
pub const ROM_VID: u16 = 0x18d1;
pub const ROM_PID: u16 = 0x4f00;

pub const BAUD: u32 = 115_200;
const READ_TIMEOUT: Duration = Duration::from_millis(200);
/// tensor-usbdl writes in 10 KiB blocks and flushes after each; the ROM has no
/// flow control, so pacing writes this way is what keeps it from dropping data.
const WRITE_BLOCK: usize = 10_240;

#[derive(Debug, Clone)]
pub struct RomDevice {
    pub port: String,
    pub serial: Option<String>,
    pub product: Option<String>,
}

/// Every serial port that belongs to a Pixel in ROM-recovery mode.
pub fn find_devices() -> Result<Vec<RomDevice>> {
    let ports = serialport::available_ports().context("listing serial ports")?;
    let mut found: Vec<RomDevice> = ports
        .into_iter()
        .filter_map(|p| match &p.port_type {
            SerialPortType::UsbPort(usb) if usb.vid == ROM_VID && usb.pid == ROM_PID => {
                Some(RomDevice {
                    port: p.port_name.clone(),
                    serial: usb.serial_number.clone(),
                    product: usb.product.clone(),
                })
            }
            _ => None,
        })
        .collect();
    // macOS exposes each CDC device twice, as /dev/tty.* and /dev/cu.*. The
    // cu (call-up) node is the one meant for initiating a connection.
    if found.iter().any(|d| d.port.starts_with("/dev/cu.")) {
        found.retain(|d| !d.port.starts_with("/dev/tty."));
    }
    Ok(found)
}

/// Serial ports that are *not* the ROM, for the `detect --all` listing.
pub fn all_ports() -> Result<Vec<SerialPortInfo>> {
    serialport::available_ports().context("listing serial ports")
}

pub fn open(port: &str) -> Result<Box<dyn SerialPort>> {
    serialport::new(port, BAUD)
        .timeout(READ_TIMEOUT)
        .open()
        .with_context(|| format!("opening {port}"))
}

/// Options for one boot attempt.
pub struct BootOptions {
    pub checksum: Checksum,
    /// Force the generation instead of guessing from the ROM's serial.
    pub generation: Option<Generation>,
    /// Send the DNW STOP command as soon as the port is open.
    pub send_stop: bool,
    /// Print every raw line from the ROM.
    pub verbose: bool,
    /// Give up if the ROM says nothing for this long.
    pub idle_timeout: Duration,
    /// How long to wait for the ROM's first byte on this connection. After a
    /// USB re-enumeration the new stage can take a few seconds to start
    /// talking, longer than the idle gap between requests.
    pub startup_timeout: Duration,
}

/// Outcome of a boot session.
#[derive(Debug)]
pub struct BootReport {
    pub serial: Option<String>,
    pub stages_sent: Vec<String>,
    /// The ROM went quiet / the port vanished after the last upload with no
    /// error. That is what success looks like: the phone is no longer a ROM
    /// device, it is booting ABL and will enumerate as fastboot.
    pub clean_exit: bool,
    /// Not a single byte arrived on this connection.
    pub silent: bool,
}

/// Drive the boot ROM: answer every `eub:req` with the right slice of the
/// right image until it stops talking to us.
pub fn boot(
    port: &mut dyn SerialPort,
    sources: &Sources,
    opts: &BootOptions,
) -> Result<BootReport> {
    let mut lines = LineReader::default();
    let mut generation = opts.generation.unwrap_or(Generation::Split);
    let mut report = BootReport {
        serial: None,
        stages_sent: Vec::new(),
        clean_exit: false,
        silent: true,
    };
    let mut pending: Option<String> = None;
    let mut last_activity = Instant::now();
    let mut buf = [0u8; 4096];

    // A newline nudges the ROM into (re)sending its banner and current request.
    port.write_all(b"\n").context("initial write")?;
    port.flush().ok();
    if opts.send_stop {
        println!("sending DNW STOP");
        port.write_all(&dnw::stop_frame())?;
        port.flush().ok();
    }

    loop {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                lines.push(&buf[..n]);
                last_activity = Instant::now();
                report.silent = false;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) if is_disconnect(&e) => {
                // The device re-enumerated. After a successful ABL hand-off this
                // is exactly what we want to see.
                report.clean_exit = !report.stages_sent.is_empty();
                println!("port closed by device ({e})");
                return Ok(report);
            }
            Err(e) => return Err(e).context("reading from ROM"),
        }

        let allowed = if report.silent {
            opts.startup_timeout
        } else {
            opts.idle_timeout
        };
        if last_activity.elapsed() > allowed {
            if report.silent {
                // Nothing ever came; let the caller decide what that means.
                return Ok(report);
            }
            if !report.stages_sent.is_empty() && pending.is_none() {
                report.clean_exit = true;
                println!(
                    "ROM has been silent for {:?} after {} stages; assuming hand-off to ABL",
                    opts.idle_timeout,
                    report.stages_sent.len()
                );
                return Ok(report);
            }
            let partial = String::from_utf8_lossy(lines.pending())
                .escape_debug()
                .to_string();
            bail!(
                "no message from the ROM for {:?} (pending request: {:?}, unparsed bytes: \"{}\"). \
                 Is the phone still in ROM-recovery mode?",
                opts.idle_timeout,
                pending,
                partial
            );
        }

        while let Some(line) = lines.next_line() {
            if opts.verbose {
                println!("  < {}", String::from_utf8_lossy(&line).escape_debug());
            }
            let Some(msg) = Message::parse(&line) else {
                continue;
            };
            match msg {
                Message::Booting { serial } => {
                    let (gen, label) = stages::guess_generation(&serial);
                    if opts.generation.is_none() {
                        generation = gen;
                    }
                    println!("ROM says hello: serial {serial} ({label})");
                    report.serial = Some(serial);
                }
                Message::Request { serial, stage } => {
                    if report.serial.is_none() && !serial.is_empty() {
                        if opts.generation.is_none() {
                            generation = stages::guess_generation(&serial).0;
                        }
                        report.serial = Some(serial);
                    }
                    if pending.as_deref() == Some(stage.as_str()) {
                        continue; // the ROM repeats its request until served
                    }
                    println!("ROM requests {stage}");
                    pending = Some(stage);
                }
                Message::ClearToSend => {
                    let Some(stage) = pending.take() else {
                        if opts.verbose {
                            println!("  (clear-to-send with nothing pending)");
                        }
                        continue;
                    };
                    let (payload, desc) = sources.payload_for(&stage, generation)?;
                    print!("  sending {desc} ... ");
                    std::io::stdout().flush().ok();
                    let started = Instant::now();
                    send(port, &payload, opts.checksum)
                        .with_context(|| format!("uploading {stage}"))?;
                    println!("done in {:.1?}", started.elapsed());
                    report.stages_sent.push(stage);
                    last_activity = Instant::now();
                }
                Message::Ack { stage } => {
                    if opts.verbose {
                        println!("  ROM acknowledged {stage}");
                    }
                }
                Message::Nak { stage } => {
                    bail!("ROM refused {stage}. Wrong device's image, or an older build than the phone's anti-rollback level?");
                }
                Message::HeaderFail { stage } => {
                    bail!(
                        "ROM rejected the {stage} header. This almost always means the \
                         bootloader image is for a different model or older than the \
                         anti-rollback version burned into the phone. Use the newest \
                         factory image for this exact model."
                    );
                }
                Message::BootFailure { trace } => {
                    let after = report
                        .stages_sent
                        .last()
                        .map(|s| format!(" after {s}"))
                        .unwrap_or_default();
                    let mut text = format!("boot ROM reported a failure{after}:");
                    for t in &trace {
                        text.push_str("\n  > ");
                        text.push_str(t);
                    }
                    bail!(text);
                }
                Message::Other(text) => {
                    if !opts.verbose {
                        println!("  ROM: {}", text.escape_debug());
                    }
                }
            }
        }
    }
}

fn send(port: &mut dyn SerialPort, payload: &[u8], checksum: Checksum) -> Result<()> {
    let frame = dnw::upload_frame(payload, checksum);
    for chunk in frame.chunks(WRITE_BLOCK) {
        port.write_all(chunk)?;
        port.flush()?;
    }
    Ok(())
}

fn is_disconnect(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        BrokenPipe | NotConnected | ConnectionAborted | ConnectionReset | NotFound
    ) || e.raw_os_error() == Some(6) // ENXIO: device vanished (macOS/Linux)
        || e.raw_os_error() == Some(5) // EIO on a gone tty
}
