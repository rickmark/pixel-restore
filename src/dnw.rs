//! Wire format of the Exynos USB Boot ("EUB") download protocol as spoken by
//! the Tensor boot ROM over its CDC-ACM serial port.
//!
//! Device → host traffic is line oriented text, e.g.
//! `exynos_usb_booting:eub:09875001abcdef0123456789`,
//! `eub:req:09875001abcdef0123456789:BL1`, a bare `C` meaning "clear to send",
//! or `bl1 header fail`. Host → device uploads are classic Samsung DNW frames:
//! `ESC D N W`, a little-endian u32 of the whole frame length, the payload,
//! and a 16-bit checksum. These shapes are what tensor-usbdl reverse engineered
//! and successfully uses on Pixel 6/7/8 hardware.

pub const OP_DNW: [u8; 4] = [0x1b, b'D', b'N', b'W'];

/// Which 16-bit trailer goes on an upload frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checksum {
    /// Sum of all payload bytes, little-endian. The historical DNW algorithm.
    Sum16,
    /// A fixed value. tensor-usbdl ships `0xFFFF` as its default and that is
    /// what the Pixel boot ROM has been observed to accept, so it is ours too.
    Fixed(u16),
}

impl Default for Checksum {
    fn default() -> Self {
        Checksum::Fixed(0xffff)
    }
}

impl Checksum {
    pub fn compute(self, payload: &[u8]) -> u16 {
        match self {
            Checksum::Fixed(v) => v,
            Checksum::Sum16 => payload
                .iter()
                .fold(0u16, |acc, &b| acc.wrapping_add(b as u16)),
        }
    }
}

/// Build an upload frame: `OP_DNW` + total length + payload + checksum.
/// The length field counts every byte of the frame, itself included
/// (4 op + 4 len + payload + 2 crc).
pub fn upload_frame(payload: &[u8], checksum: Checksum) -> Vec<u8> {
    let total = 4 + 4 + payload.len() + 2;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&OP_DNW);
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&checksum.compute(payload).to_le_bytes());
    out
}

/// The DNW STOP command tensor-usbdl sends with `--stop`: op code, a zero
/// argument word, no payload, trailer `01 00`.
pub fn stop_frame() -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    out.extend_from_slice(&OP_DNW);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&[0x01, 0x00]);
    out
}

/// One line received from the boot ROM, split on `:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// `C`: the ROM is ready to receive the stage it last requested.
    ClearToSend,
    /// `exynos_usb_booting:<sub>:<serial>` — the ROM announcing itself.
    Booting { serial: String },
    /// `eub:req:<serial>:<STAGE>` — please send this stage.
    Request { serial: String, stage: String },
    /// `eub:ack:<serial>:<STAGE>`.
    Ack { stage: String },
    /// `eub:nak:<serial>:<STAGE>`.
    Nak { stage: String },
    /// `<stage> header fail` — the ROM rejected what we sent.
    HeaderFail { stage: String },
    /// `irom_booting_failure:<sub>:<trace>` — a NUL-separated failure trace.
    BootFailure { trace: Vec<String> },
    /// Anything we do not recognise, kept verbatim for the log.
    Other(String),
}

impl Message {
    pub fn parse(line: &[u8]) -> Option<Message> {
        if line.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(line);
        let text = text.trim_matches(|c| c == '\r' || c == '\n');
        if text.is_empty() {
            return None;
        }
        if let Some(stage) = text.strip_suffix(" header fail") {
            return Some(Message::HeaderFail {
                stage: stage.trim().to_string(),
            });
        }
        let mut parts = text.splitn(4, ':');
        let cmd = parts.next().unwrap_or("");
        let sub = parts.next().unwrap_or("");
        let dev = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("");
        Some(match cmd {
            "C" => Message::ClearToSend,
            "exynos_usb_booting" => Message::Booting {
                serial: dev.to_string(),
            },
            "eub" => match sub {
                "req" => Message::Request {
                    serial: dev.to_string(),
                    stage: arg.to_ascii_uppercase(),
                },
                "ack" => Message::Ack {
                    stage: arg.to_ascii_uppercase(),
                },
                "nak" => Message::Nak {
                    stage: arg.to_ascii_uppercase(),
                },
                _ => Message::Other(text.to_string()),
            },
            "irom_booting_failure" => {
                // The trace rides in the third field as NUL-separated strings
                // with an empty first and last element.
                let trace = dev
                    .split('\0')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                Message::BootFailure { trace }
            }
            _ => Message::Other(text.to_string()),
        })
    }
}

/// Accumulates raw serial bytes and yields complete lines, since the ROM's
/// messages arrive in arbitrary read-sized chunks.
#[derive(Default)]
pub struct LineReader {
    buf: Vec<u8>,
}

impl LineReader {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete line, without its terminator. `None` means no
    /// full line is buffered yet.
    pub fn next_line(&mut self) -> Option<Vec<u8>> {
        loop {
            let pos = self.buf.iter().position(|&b| b == b'\n' || b == b'\r')?;
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            if !line.is_empty() {
                return Some(line.to_vec());
            }
        }
    }

    /// Bytes buffered that do not yet form a line, for diagnostics.
    pub fn pending(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout_matches_tensor_usbdl() {
        let payload = [0x10u8, 0x20, 0x30];
        let f = upload_frame(&payload, Checksum::Fixed(0xffff));
        assert_eq!(&f[0..4], &OP_DNW);
        assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()), 13);
        assert_eq!(&f[8..11], &payload);
        assert_eq!(&f[11..13], &[0xff, 0xff]);
        assert_eq!(f.len(), 13);

        let f = upload_frame(&[0xff, 0x02], Checksum::Sum16);
        assert_eq!(&f[f.len() - 2..], &[0x01, 0x01]); // 0xff + 0x02 = 0x101
    }

    #[test]
    fn stop_frame_layout() {
        assert_eq!(stop_frame(), vec![0x1b, b'D', b'N', b'W', 0, 0, 0, 0, 1, 0]);
    }

    #[test]
    fn parses_messages() {
        assert_eq!(Message::parse(b"C"), Some(Message::ClearToSend));
        assert_eq!(
            Message::parse(b"exynos_usb_booting:eub:09845001cddf16d00bd4"),
            Some(Message::Booting {
                serial: "09845001cddf16d00bd4".into()
            })
        );
        assert_eq!(
            Message::parse(b"eub:req:09845001cddf16d00bd4:bl1"),
            Some(Message::Request {
                serial: "09845001cddf16d00bd4".into(),
                stage: "BL1".into()
            })
        );
        assert_eq!(
            Message::parse(b"eub:ack:0984:DPM"),
            Some(Message::Ack {
                stage: "DPM".into()
            })
        );
        assert_eq!(
            Message::parse(b"bl1 header fail"),
            Some(Message::HeaderFail {
                stage: "bl1".into()
            })
        );
        assert_eq!(
            Message::parse(b"irom_booting_failure:x:\0one\0two\0"),
            Some(Message::BootFailure {
                trace: vec!["one".into(), "two".into()]
            })
        );
        assert_eq!(Message::parse(b"\r\n"), None);
        assert!(matches!(Message::parse(b"weird"), Some(Message::Other(_))));
    }

    #[test]
    fn line_reader_reassembles_chunks() {
        let mut r = LineReader::default();
        r.push(b"eub:re");
        assert_eq!(r.next_line(), None);
        r.push(b"q:dev:BL1\r\n\nC\n");
        assert_eq!(r.next_line().as_deref(), Some(&b"eub:req:dev:BL1"[..]));
        assert_eq!(r.next_line().as_deref(), Some(&b"C"[..]));
        assert_eq!(r.next_line(), None);
        r.push(b"partial");
        assert_eq!(r.pending(), b"partial");
    }
}
