//! Parser for Google's FBPK ("FastBoot PacK") v2 container, which is the
//! format of the `bootloader-<device>-<version>.img` file inside a Pixel
//! factory image. Layout follows the reference `fbpack.py` / `fbpacktool.py`
//! published on source.android.com: a 112-byte little-endian header, then
//! `total_entries` contiguous 104-byte entry headers, then the entry payloads
//! at the absolute file offsets each entry header names.

use anyhow::{bail, ensure, Context, Result};

pub const FBPK_MAGIC: u32 = 0x4b50_4246; // "FBPK"
pub const FBPK_VERSION: u32 = 2;
pub const HEADER_SIZE: usize = 112;
pub const ENTRY_SIZE: usize = 104;

pub const ENTRY_PARTITION_TABLE: u32 = 0;
pub const ENTRY_PARTITION_DATA: u32 = 1;
pub const ENTRY_SIDELOAD_DATA: u32 = 2;

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: u32,
    pub name: String,
    pub product: String,
    pub offset: u64,
    pub size: u64,
    pub slotted: bool,
    pub crc32: u32,
}

impl Entry {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            ENTRY_PARTITION_TABLE => "table",
            ENTRY_PARTITION_DATA => "data",
            ENTRY_SIDELOAD_DATA => "sideload",
            _ => "unknown",
        }
    }
}

#[derive(Debug)]
pub struct Pack {
    pub version: u32,
    pub platform: String,
    pub pack_version: String,
    pub slot_type: u32,
    pub data_align: u32,
    pub total_size: u32,
    pub entries: Vec<Entry>,
    data: Vec<u8>,
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn u64_at(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn cstr_at(buf: &[u8], off: usize, len: usize) -> String {
    let raw = &buf[off..off + len];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(len);
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

impl Pack {
    /// Parse a complete FBPK v2 image held in memory.
    pub fn parse(data: Vec<u8>) -> Result<Pack> {
        ensure!(
            data.len() >= HEADER_SIZE,
            "file is {} bytes, too small for an FBPK header",
            data.len()
        );
        let magic = u32_at(&data, 0);
        ensure!(
            magic == FBPK_MAGIC,
            "bad magic {:#010x}: not an FBPK bootloader image",
            magic
        );
        let version = u32_at(&data, 4);
        let header_size = u32_at(&data, 8) as usize;
        let entry_header_size = u32_at(&data, 12) as usize;
        if version != FBPK_VERSION {
            bail!("FBPK version {version} is not supported (only v2)");
        }
        ensure!(
            header_size == HEADER_SIZE && entry_header_size == ENTRY_SIZE,
            "unexpected header sizes ({header_size}/{entry_header_size})"
        );
        let platform = cstr_at(&data, 16, 16);
        let pack_version = cstr_at(&data, 32, 64);
        let slot_type = u32_at(&data, 96);
        let data_align = u32_at(&data, 100);
        let total_entries = u32_at(&data, 104) as usize;
        let total_size = u32_at(&data, 108);

        let table_end = header_size + total_entries * entry_header_size;
        ensure!(
            data.len() >= table_end,
            "truncated: {total_entries} entries need {table_end} bytes, have {}",
            data.len()
        );

        let mut entries = Vec::with_capacity(total_entries);
        for i in 0..total_entries {
            let base = header_size + i * entry_header_size;
            let e = Entry {
                kind: u32_at(&data, base),
                name: cstr_at(&data, base + 4, 36),
                product: cstr_at(&data, base + 40, 40),
                offset: u64_at(&data, base + 80),
                size: u64_at(&data, base + 88),
                slotted: u32_at(&data, base + 96) != 0,
                crc32: u32_at(&data, base + 100),
            };
            let end = e
                .offset
                .checked_add(e.size)
                .with_context(|| format!("entry '{}' has an absurd size", e.name))?;
            ensure!(
                end <= data.len() as u64,
                "entry '{}' ({} bytes at {:#x}) runs past end of file",
                e.name,
                e.size,
                e.offset
            );
            entries.push(e);
        }

        Ok(Pack {
            version,
            platform,
            pack_version,
            slot_type,
            data_align,
            total_size,
            entries,
            data,
        })
    }

    pub fn entry_data(&self, e: &Entry) -> &[u8] {
        &self.data[e.offset as usize..(e.offset + e.size) as usize]
    }

    /// CRC32 (IEEE) over the entry payload compared with the stored value.
    pub fn verify_crc(&self, e: &Entry) -> bool {
        e.crc32 == 0 || crc32fast::hash(self.entry_data(e)) == e.crc32
    }

    /// Find a data entry by partition name. Matching is case-insensitive and
    /// tolerates an `_a` / `_b` slot suffix on either side, so `abl`, `abl_a`
    /// and `ABL` all resolve to the same image. Partition-table entries are
    /// never returned.
    pub fn find(&self, name: &str) -> Option<&Entry> {
        let lower = name.to_ascii_lowercase();
        let want = strip_slot(&lower);
        self.entries
            .iter()
            .filter(|e| e.kind != ENTRY_PARTITION_TABLE)
            .find(|e| strip_slot(&e.name.to_ascii_lowercase()) == want)
    }

    pub fn data_entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|e| e.kind != ENTRY_PARTITION_TABLE)
    }
}

fn strip_slot(name: &str) -> &str {
    name.strip_suffix("_a")
        .or_else(|| name.strip_suffix("_b"))
        .unwrap_or(name)
}

/// Load an FBPK image from either a raw `bootloader-*.img` or a factory-image
/// ZIP that contains one at any depth.
pub fn load(path: &std::path::Path) -> Result<Pack> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() >= 4 && u32_at(&bytes, 0) == FBPK_MAGIC {
        return Pack::parse(bytes);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return load_from_zip(path, bytes);
    }
    bail!(
        "{} is neither an FBPK bootloader image nor a ZIP",
        path.display()
    )
}

fn load_from_zip(path: &std::path::Path, bytes: Vec<u8>) -> Result<Pack> {
    let cursor = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor)
        .with_context(|| format!("opening {} as a ZIP", path.display()))?;
    let candidate = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| (i, f.name().to_string())))
        .find(|(_, n)| {
            let base = n.rsplit('/').next().unwrap_or(n);
            base.starts_with("bootloader") && base.ends_with(".img")
        });
    let Some((idx, name)) = candidate else {
        bail!(
            "{} contains no bootloader-*.img (is it the factory image ZIP, not the OTA?)",
            path.display()
        );
    };
    eprintln!("using {name} from {}", path.display());
    let mut f = zip.by_index(idx)?;
    let mut out = Vec::with_capacity(f.size() as usize);
    std::io::copy(&mut f, &mut out)?;
    Pack::parse(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a synthetic FBPK v2 container the same way fbpacktool's `create`
    /// does: header, contiguous entry table, then payloads aligned to
    /// `data_align`.
    pub fn build(entries: &[(&str, u32, bool, &[u8])]) -> Vec<u8> {
        let align = 16usize;
        let mut out = vec![0u8; HEADER_SIZE + entries.len() * ENTRY_SIZE];
        out[0..4].copy_from_slice(&FBPK_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&FBPK_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        out[12..16].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        out[16..16 + 7].copy_from_slice(b"zumapro");
        out[32..32 + 9].copy_from_slice(b"test-1.0 ");
        out[96..100].copy_from_slice(&1u32.to_le_bytes());
        out[100..104].copy_from_slice(&(align as u32).to_le_bytes());
        out[104..108].copy_from_slice(&(entries.len() as u32).to_le_bytes());

        for (i, (name, kind, slotted, payload)) in entries.iter().enumerate() {
            while !out.len().is_multiple_of(align) {
                out.push(0);
            }
            let offset = out.len() as u64;
            out.extend_from_slice(payload);
            let base = HEADER_SIZE + i * ENTRY_SIZE;
            out[base..base + 4].copy_from_slice(&kind.to_le_bytes());
            out[base + 4..base + 4 + name.len()].copy_from_slice(name.as_bytes());
            out[base + 40..base + 40 + 6].copy_from_slice(b"komodo");
            out[base + 80..base + 88].copy_from_slice(&offset.to_le_bytes());
            out[base + 88..base + 96].copy_from_slice(&(payload.len() as u64).to_le_bytes());
            out[base + 96..base + 100].copy_from_slice(&(*slotted as u32).to_le_bytes());
            out[base + 100..base + 104].copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
        }
        let total = out.len() as u32;
        out[108..112].copy_from_slice(&total.to_le_bytes());
        out
    }

    #[test]
    fn parses_synthetic_pack() {
        let img = build(&[
            ("partition_table", ENTRY_PARTITION_TABLE, false, b"ptable"),
            ("bl1", ENTRY_PARTITION_DATA, true, &[1u8; 4096 + 17]),
            ("abl", ENTRY_PARTITION_DATA, true, b"abl-body"),
        ]);
        let pack = Pack::parse(img).unwrap();
        assert_eq!(pack.version, 2);
        assert_eq!(pack.platform, "zumapro");
        assert_eq!(pack.pack_version, "test-1.0 ");
        assert_eq!(pack.data_align, 16);
        assert_eq!(pack.entries.len(), 3);

        let bl1 = pack.find("BL1_a").expect("bl1 present");
        assert_eq!(bl1.size, 4096 + 17);
        assert!(bl1.slotted);
        assert_eq!(bl1.product, "komodo");
        assert!(pack.verify_crc(bl1));
        assert_eq!(pack.entry_data(bl1)[0], 1);

        let abl = pack.find("abl").unwrap();
        assert_eq!(pack.entry_data(abl), b"abl-body");
        assert!(
            pack.find("partition_table").is_none(),
            "tables are not images"
        );
        assert!(pack.find("tzsw").is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Pack::parse(vec![0u8; 10]).is_err());
        assert!(Pack::parse(vec![0u8; 200]).is_err());
        let mut img = build(&[("bl1", ENTRY_PARTITION_DATA, false, b"x")]);
        img[4..8].copy_from_slice(&1u32.to_le_bytes()); // version 1
        assert!(Pack::parse(img).is_err());
    }

    #[test]
    fn rejects_entry_past_eof() {
        let mut img = build(&[("bl1", ENTRY_PARTITION_DATA, false, b"xyz")]);
        let base = HEADER_SIZE;
        img[base + 88..base + 96].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert!(Pack::parse(img).is_err());
    }

    #[test]
    fn detects_corrupt_payload() {
        let mut img = build(&[("bl1", ENTRY_PARTITION_DATA, false, b"hello")]);
        let last = img.len() - 1;
        img[last] ^= 0xff;
        let pack = Pack::parse(img).unwrap();
        assert!(!pack.verify_crc(pack.find("bl1").unwrap()));
    }
}
