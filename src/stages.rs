//! Mapping from the stage names the boot ROM asks for (`BL1`, `EPBL`, `ABLB`,
//! ...) to partitions inside `bootloader.img`, plus which slice of that
//! partition to send.
//!
//! Every stage image in the pack is a 4096-byte signed header followed by a
//! code body. The ROM sometimes asks for the whole image in one request and
//! sometimes for the header and body separately (the body request is the
//! header's name with a trailing `B`: `ABL` then `ABLB`, `BL2` then `BL2B`).
//! The split table below is the order observed by tensor-usbdl on Pixel 7 and
//! Pixel 8 hardware; a name the table does not know falls through to a
//! best-effort guess so a newer SoC can still be served.

use crate::fbpk::Pack;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

pub const HEADER_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Full,
    Header,
    Body,
}

/// Which SoC generation the connected ROM belongs to. Decides the one known
/// per-generation difference: Pixel 7 (gs201) wants the whole PBL when it asks
/// for `EPBL`, Pixel 8 (zuma) wants only the header and then asks for `EPBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    /// Pixel 6 / 7 style: `EPBL` is the whole PBL.
    Legacy,
    /// Pixel 8 and later: `EPBL` is the header, `EPBB` the body.
    Split,
}

/// Guess the generation from the serial the ROM reports in
/// `exynos_usb_booting`. Known prefixes (characters 1..5) are 9845 = Pixel 6,
/// 9855 = Pixel 7, 9865 = Pixel 8. Anything newer or unknown is assumed to
/// behave like Pixel 8, since that is the more recent design.
pub fn guess_generation(serial: &str) -> (Generation, &'static str) {
    let prefix = serial.get(1..5).unwrap_or("");
    match prefix {
        "9845" => (Generation::Legacy, "Pixel 6 series (gs101)"),
        "9855" => (Generation::Legacy, "Pixel 7 series (gs201)"),
        "9865" => (Generation::Split, "Pixel 8 series (zuma)"),
        _ => (
            Generation::Split,
            "unknown series, assuming Pixel 8+ behaviour",
        ),
    }
}

/// Resolve a ROM request to (partition name, part of the image).
pub fn resolve(stage: &str, generation: Generation) -> Option<(&'static str, Part)> {
    use Part::*;
    Some(match stage {
        "BL1" => ("bl1", Full),
        "DPM" => ("dpm", Full),
        "EPBL" => (
            "pbl",
            if generation == Generation::Split {
                Header
            } else {
                Full
            },
        ),
        "EPBB" => ("pbl", Body),
        "BL2" => ("bl2", Header),
        "BL2B" => ("bl2", Body),
        "GSA1" => ("gsa", Full),
        "GSAF" => ("gsa", Full),
        "ABL" => ("abl", Header),
        "ABLB" => ("abl", Body),
        "TZSW" => ("tzsw", Header),
        "TZSB" => ("tzsw", Body),
        "LDFW" => ("ldfw", Header),
        "LDFB" => ("ldfw", Body),
        "BL31" => ("bl31", Header),
        "BL3B" => ("bl31", Body),
        "GCF" => ("gcf", Header),
        "GCFB" => ("gcf", Body),
        _ => return None,
    })
}

/// Alternative partition names to try when the first choice is absent from
/// the pack. `gsa_bl1` is what some factory images call the GSA first stage.
fn aliases(partition: &str) -> &'static [&'static str] {
    match partition {
        "gsa" => &["gsa", "gsa_bl1", "gsa1"],
        "pbl" => &["pbl", "epbl"],
        _ => &[],
    }
}

/// Where stage bytes come from: the FBPK pack, loose files, or synthesized.
pub struct Sources {
    pub pack: Option<Pack>,
    /// Loose image files keyed by lowercase partition name (`bl1`, `abl`, ...).
    pub files: HashMap<String, Vec<u8>>,
    /// Explicit `REQUEST=partition` overrides from the command line.
    pub remap: HashMap<String, String>,
}

impl Sources {
    fn partition_bytes(&self, partition: &str) -> Option<&[u8]> {
        if let Some(bytes) = self.files.get(partition) {
            return Some(bytes);
        }
        let pack = self.pack.as_ref()?;
        let names: Vec<&str> = if aliases(partition).is_empty() {
            vec![partition]
        } else {
            aliases(partition).to_vec()
        };
        names
            .iter()
            .find_map(|n| pack.find(n))
            .map(|e| pack.entry_data(e))
    }

    /// Produce the exact bytes to upload for a ROM request. Returns the
    /// payload plus a human-readable description of where it came from.
    pub fn payload_for(&self, stage: &str, generation: Generation) -> Result<(Vec<u8>, String)> {
        let (partition, part) = match self.remap.get(stage) {
            Some(p) => (
                p.as_str(),
                resolve(stage, generation)
                    .map(|r| r.1)
                    .unwrap_or(Part::Full),
            ),
            None => resolve(stage, generation).ok_or_else(|| {
                anyhow!(
                    "the ROM asked for '{stage}', which I don't know how to serve; \
                     pass --map {stage}=<partition> to tell me which image it wants"
                )
            })?,
        };

        let bytes = match self.partition_bytes(partition) {
            Some(b) => b,
            None if partition == "dpm" => {
                // tensor-usbdl sends 4 KiB of zeros when no DPM image exists
                // and Pixel 7/8 ROMs accept it, so do the same.
                return Ok((vec![0u8; HEADER_LEN], "zeroed 4096-byte DPM".into()));
            }
            None => {
                return Err(anyhow!(
                    "no '{partition}' image available for request '{stage}' \
                     (not in bootloader.img and no --stage {partition}=<file> given)"
                ))
            }
        };

        let desc_part = match part {
            Part::Full => "full image",
            Part::Header => "4096-byte header",
            Part::Body => "body after header",
        };
        let slice = match part {
            Part::Full => bytes,
            Part::Header => bytes
                .get(..HEADER_LEN)
                .ok_or_else(|| anyhow!("'{partition}' is shorter than its 4096-byte header"))?,
            Part::Body => bytes
                .get(HEADER_LEN..)
                .ok_or_else(|| anyhow!("'{partition}' has no body after its header"))?,
        };
        Ok((
            slice.to_vec(),
            format!("{partition} ({desc_part}, {} bytes)", slice.len()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fbpk::{self, ENTRY_PARTITION_DATA};

    fn sources() -> Sources {
        let mut abl = vec![0xAAu8; HEADER_LEN];
        abl.extend_from_slice(&[0xBB; 100]);
        let img = fbpk::tests::build(&[
            ("bl1_a", ENTRY_PARTITION_DATA, true, &[1u8; HEADER_LEN + 8]),
            ("abl_a", ENTRY_PARTITION_DATA, true, &abl),
            ("gsa_bl1", ENTRY_PARTITION_DATA, false, &[7u8; 10]),
            ("pbl", ENTRY_PARTITION_DATA, false, &[9u8; HEADER_LEN + 3]),
        ]);
        Sources {
            pack: Some(Pack::parse(img).unwrap()),
            files: HashMap::new(),
            remap: HashMap::new(),
        }
    }

    #[test]
    fn serves_full_header_and_body() {
        let s = sources();
        let (bl1, _) = s.payload_for("BL1", Generation::Split).unwrap();
        assert_eq!(bl1.len(), HEADER_LEN + 8);
        let (hdr, _) = s.payload_for("ABL", Generation::Split).unwrap();
        assert_eq!(hdr.len(), HEADER_LEN);
        assert!(hdr.iter().all(|&b| b == 0xAA));
        let (body, _) = s.payload_for("ABLB", Generation::Split).unwrap();
        assert_eq!(body, vec![0xBB; 100]);
    }

    #[test]
    fn epbl_depends_on_generation() {
        let s = sources();
        assert_eq!(
            s.payload_for("EPBL", Generation::Split).unwrap().0.len(),
            HEADER_LEN
        );
        assert_eq!(
            s.payload_for("EPBL", Generation::Legacy).unwrap().0.len(),
            HEADER_LEN + 3
        );
        assert_eq!(s.payload_for("EPBB", Generation::Split).unwrap().0.len(), 3);
    }

    #[test]
    fn gsa_alias_and_dpm_fallback() {
        let s = sources();
        assert_eq!(
            s.payload_for("GSA1", Generation::Split).unwrap().0,
            vec![7u8; 10]
        );
        let (dpm, desc) = s.payload_for("DPM", Generation::Split).unwrap();
        assert_eq!(dpm, vec![0u8; HEADER_LEN]);
        assert!(desc.contains("zeroed"));
    }

    #[test]
    fn missing_and_unknown_requests_error() {
        let s = sources();
        assert!(s.payload_for("TZSW", Generation::Split).is_err());
        assert!(s.payload_for("WHAT", Generation::Split).is_err());
    }

    #[test]
    fn remap_and_loose_files_win() {
        let mut s = sources();
        s.remap.insert("WHAT".into(), "gsa".into());
        assert_eq!(
            s.payload_for("WHAT", Generation::Split).unwrap().0,
            vec![7u8; 10]
        );
        s.files.insert("bl1".into(), vec![5u8; 3]);
        assert_eq!(
            s.payload_for("BL1", Generation::Split).unwrap().0,
            vec![5u8; 3]
        );
    }

    #[test]
    fn generation_guess() {
        assert_eq!(guess_generation("09855001abcd").0, Generation::Legacy);
        assert_eq!(guess_generation("09865001abcd").0, Generation::Split);
        assert_eq!(guess_generation("09875001abcd").0, Generation::Split);
        assert_eq!(guess_generation("").0, Generation::Split);
    }
}
