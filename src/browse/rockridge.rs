//! Rock Ridge / SUSP (System Use Sharing Protocol) parsing for ISO 9660.
//!
//! Rock Ridge (IEEE P1282) records POSIX metadata, long/Unicode-free names, and
//! symbolic links inside the "System Use" area that trails each directory
//! record. SUSP (IEEE P1281) is the framing: a sequence of
//! `[signature(2)][length(1)][version(1)][data…]` entries, with `CE` pointing to
//! a continuation area elsewhere on the disc when the entries overflow the
//! directory record.
//!
//! This module parses the entries opticaldiscs surfaces on [`FileEntry`]:
//! `PX` (mode/uid/gid), `NM` (alternate name), `SL` (symlink target), and `TF`
//! (timestamps). `CE` continuation areas are followed via the supplied reader.
//!
//! [`FileEntry`]: super::entry::FileEntry

use super::entry::PosixMetadata;
use crate::iso9660::{Iso9660DateTime, PvdDateTime};
use crate::sector_reader::{SectorReader, SECTOR_SIZE};

/// Metadata extracted from a directory record's Rock Ridge System Use area.
/// All fields are `None`/empty when the disc carries no Rock Ridge data.
#[derive(Debug, Default, Clone)]
pub struct RockRidge {
    /// Alternate name from `NM` entries (concatenated across continuations).
    pub name: Option<String>,
    /// POSIX ownership/permissions from a `PX` entry.
    pub posix: Option<PosixMetadata>,
    /// Symlink target assembled from `SL` component records.
    pub symlink_target: Option<String>,
    /// `TF` creation time.
    pub created: Option<Iso9660DateTime>,
    /// `TF` modification time.
    pub modified: Option<Iso9660DateTime>,
    /// `TF` access time.
    pub accessed: Option<Iso9660DateTime>,
}

/// Read the little-endian half of an 8-byte ISO 9660 "both-endian" `u32`.
fn both_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Convert a 17-byte-form [`PvdDateTime`] (used by `TF` long form) to the
/// 7-byte [`Iso9660DateTime`] our API exposes (hundredths are dropped).
fn pvd_to_iso(dt: PvdDateTime) -> Iso9660DateTime {
    Iso9660DateTime {
        years_since_1900: (dt.year.saturating_sub(1900)) as u8,
        month: dt.month,
        day: dt.day,
        hour: dt.hour,
        minute: dt.minute,
        second: dt.second,
        gmt_offset_quarter_hours: dt.gmt_offset_quarter_hours,
    }
}

/// True if `system_use` contains any recognizable SUSP/Rock Ridge entry. Used to
/// decide whether to browse the primary tree (with Rock Ridge) in preference to
/// a Joliet tree.
pub fn detect(system_use: &[u8]) -> bool {
    let mut p = 0usize;
    while p + 4 <= system_use.len() {
        let sig = &system_use[p..p + 2];
        let len = system_use[p + 2] as usize;
        if len < 4 || p + len > system_use.len() {
            break;
        }
        if matches!(
            sig,
            b"SP" | b"ER" | b"PX" | b"NM" | b"SL" | b"TF" | b"RR" | b"CE"
        ) {
            return true;
        }
        p += len;
    }
    false
}

/// Parse the Rock Ridge System Use area, following `CE` continuation areas via
/// `reader`. Never fails: unrecognized or malformed entries are skipped.
pub fn parse(system_use: &[u8], reader: &mut dyn SectorReader) -> RockRidge {
    let mut out = RockRidge::default();

    let mut name = String::new();
    let mut have_name = false;
    let mut sl = String::new();
    let mut sl_pending_continue = false; // previous SL component had CONTINUE
    let mut have_sl = false;

    // Areas to scan: the record's System Use, plus any CE continuation areas.
    let mut areas: Vec<Vec<u8>> = vec![system_use.to_vec()];
    let mut ce_reads = 0u32;
    let mut idx = 0usize;

    while idx < areas.len() {
        let area = areas[idx].clone();
        idx += 1;
        let mut p = 0usize;
        while p + 4 <= area.len() {
            let sig = [area[p], area[p + 1]];
            let len = area[p + 2] as usize;
            if len < 4 || p + len > area.len() {
                break;
            }
            let data = &area[p + 4..p + len];
            match &sig {
                b"ST" => break, // System-use terminator for this area.
                b"NM" => {
                    if !data.is_empty() {
                        let flags = data[0];
                        // bit1 = CURRENT ("."), bit2 = PARENT ("..") — ignore content.
                        if flags & 0x06 == 0 {
                            name.push_str(&String::from_utf8_lossy(&data[1..]));
                        }
                        have_name = true;
                    }
                }
                b"PX" => {
                    // st_mode(8) st_nlink(8) st_uid(8) st_gid(8) [st_ino(8)]
                    if data.len() >= 32 {
                        out.posix = Some(PosixMetadata {
                            mode: both_u32(&data[0..8]),
                            uid: both_u32(&data[16..24]),
                            gid: both_u32(&data[24..32]),
                        });
                    }
                }
                b"TF" => parse_tf(data, &mut out),
                b"SL" => {
                    have_sl = true;
                    parse_sl(data, &mut sl, &mut sl_pending_continue);
                }
                // block(both,8) offset(both,8) length(both,8)
                b"CE" if data.len() >= 24 && ce_reads < 16 => {
                    let block = both_u32(&data[0..8]);
                    let ce_off = both_u32(&data[8..16]);
                    let ce_len = both_u32(&data[16..24]);
                    if ce_len > 0 {
                        ce_reads += 1;
                        let byte_off = block as u64 * SECTOR_SIZE + ce_off as u64;
                        if let Ok(cont) = reader.read_bytes(byte_off, ce_len as usize) {
                            areas.push(cont);
                        }
                    }
                }
                // SP / ER / PD / RR / PL / CL / RE and anything else: skip.
                _ => {}
            }
            p += len;
        }
    }

    if have_name && !name.is_empty() {
        out.name = Some(name);
    }
    if have_sl {
        out.symlink_target = Some(sl);
    }
    out
}

/// Parse a `TF` (timestamp) entry into `out`. `data[0]` is a flags byte whose
/// bits select which timestamps follow (creation, modify, access, …); bit 7
/// (`LONG_FORM`) selects the 17-byte ASCII form over the 7-byte binary form.
fn parse_tf(data: &[u8], out: &mut RockRidge) {
    if data.is_empty() {
        return;
    }
    let flags = data[0];
    let long = flags & 0x80 != 0;
    let stamp_len = if long { 17 } else { 7 };
    let mut off = 1usize;
    // Bit order: 0=creation, 1=modify, 2=access, 3=attributes, 4=backup,
    // 5=expiration, 6=effective.
    for bit in 0..7 {
        if flags & (1 << bit) == 0 {
            continue;
        }
        if off + stamp_len > data.len() {
            break;
        }
        let dt = if long {
            PvdDateTime::parse(&data[off..off + 17]).map(pvd_to_iso)
        } else {
            Iso9660DateTime::parse(&data[off..off + 7])
        };
        match bit {
            0 => out.created = dt,
            1 => out.modified = dt,
            2 => out.accessed = dt,
            _ => {}
        }
        off += stamp_len;
    }
}

/// Parse an `SL` (symlink) entry, appending its component records to `target`.
/// `pending_continue` carries the "component continues" state across `SL`
/// entries and across component records within one entry.
fn parse_sl(data: &[u8], target: &mut String, pending_continue: &mut bool) {
    if data.is_empty() {
        return;
    }
    // data[0] = SL flags (bit0 = the link continues in a further SL entry); the
    // component list follows.
    let mut q = 1usize;
    while q + 2 <= data.len() {
        let comp_flags = data[q];
        let comp_len = data[q + 1] as usize;
        if q + 2 + comp_len > data.len() {
            break;
        }
        let content = &data[q + 2..q + 2 + comp_len];

        // Component-type bits: 1=CURRENT ".", 2=PARENT "..", 3=ROOT "/".
        let piece = if comp_flags & 0x08 != 0 {
            "/".to_string()
        } else if comp_flags & 0x02 != 0 {
            ".".to_string()
        } else if comp_flags & 0x04 != 0 {
            "..".to_string()
        } else {
            String::from_utf8_lossy(content).into_owned()
        };

        if *pending_continue {
            // Previous component had CONTINUE (bit0): join without a separator.
            target.push_str(&piece);
        } else if piece == "/" {
            // Root component: start the path at "/".
            if !target.ends_with('/') {
                target.push('/');
            }
        } else {
            if !target.is_empty() && !target.ends_with('/') {
                target.push('/');
            }
            target.push_str(&piece);
        }

        // bit0 = this component continues in the next component record.
        *pending_continue = comp_flags & 0x01 != 0;
        q += 2 + comp_len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::sector_reader::SECTOR_SIZE;

    /// A `SectorReader` over an in-memory image, for exercising `CE` follows.
    struct MemReader(Vec<u8>);
    impl SectorReader for MemReader {
        fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
            let start = (lba * SECTOR_SIZE) as usize;
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            if start < self.0.len() {
                let end = (start + SECTOR_SIZE as usize).min(self.0.len());
                buf[..end - start].copy_from_slice(&self.0[start..end]);
            }
            Ok(buf)
        }
    }

    /// A reader that always errors — for tests that never hit a `CE` entry.
    struct NoReader;
    impl SectorReader for NoReader {
        fn read_sector(&mut self, _lba: u64) -> Result<Vec<u8>> {
            Err(crate::error::OpticaldiscsError::Parse("no reader".into()))
        }
    }

    /// Build a SUSP entry: signature(2) + length(1) + version(1) + data.
    fn entry(sig: &[u8; 2], data: &[u8]) -> Vec<u8> {
        let mut v = vec![sig[0], sig[1], (4 + data.len()) as u8, 1];
        v.extend_from_slice(data);
        v
    }

    /// Encode an ISO 9660 "both-endian" u32 (LE then BE).
    fn both(n: u32) -> Vec<u8> {
        let mut v = n.to_le_bytes().to_vec();
        v.extend_from_slice(&n.to_be_bytes());
        v
    }

    #[test]
    fn detect_recognizes_susp() {
        assert!(detect(&entry(b"PX", &[0u8; 32])));
        assert!(detect(&entry(b"SP", &[0xBE, 0xEF, 0])));
        assert!(!detect(b""));
        assert!(!detect(&[0xAA, 0xBB, 0x04, 0x01])); // unknown signature
    }

    #[test]
    fn parse_px_posix() {
        let mut data = Vec::new();
        data.extend(both(0o100_644)); // st_mode
        data.extend(both(1)); // st_nlink
        data.extend(both(501)); // st_uid
        data.extend(both(20)); // st_gid
        let rr = parse(&entry(b"PX", &data), &mut NoReader);
        let px = rr.posix.expect("PX parsed");
        assert_eq!(px.mode, 0o100_644);
        assert_eq!(px.uid, 501);
        assert_eq!(px.gid, 20);
        assert_eq!(px.permission_bits(), 0o644);
    }

    #[test]
    fn parse_tf_short_form_timestamps() {
        // flags: creation(bit0) + modify(bit1) + access(bit2), short form.
        let mut data = vec![0b0000_0111];
        // creation
        data.extend_from_slice(&[97, 3, 18, 16, 45, 47, 0]);
        // modify
        data.extend_from_slice(&[98, 6, 1, 8, 0, 0, 0]);
        // access
        data.extend_from_slice(&[99, 12, 31, 23, 59, 59, 0]);
        let rr = parse(&entry(b"TF", &data), &mut NoReader);
        assert_eq!(rr.created.unwrap().year(), 1997);
        assert_eq!(rr.modified.unwrap().month, 6);
        assert_eq!(rr.accessed.unwrap().second, 59);
    }

    #[test]
    fn parse_nm_name() {
        let mut data = vec![0u8]; // flags: no continue, not current/parent
        data.extend_from_slice(b"long file name.txt");
        let rr = parse(&entry(b"NM", &data), &mut NoReader);
        assert_eq!(rr.name.as_deref(), Some("long file name.txt"));
    }

    #[test]
    fn parse_sl_symlink() {
        // SL: flags(0), then components: "usr", "bin", "sh" → /usr/bin/sh-ish
        // Each component record: comp_flags(1) + comp_len(1) + content.
        let mut data = vec![0u8]; // SL flags
        for comp in ["usr", "bin", "sh"] {
            data.push(0); // component flags: normal
            data.push(comp.len() as u8);
            data.extend_from_slice(comp.as_bytes());
        }
        let rr = parse(&entry(b"SL", &data), &mut NoReader);
        assert_eq!(rr.symlink_target.as_deref(), Some("usr/bin/sh"));
    }

    #[test]
    fn parse_sl_absolute_root() {
        // ROOT component (flag bit3) then "etc", "hosts".
        let mut data = vec![0u8];
        data.extend_from_slice(&[0x08, 0]); // ROOT, zero-length
        for comp in ["etc", "hosts"] {
            data.push(0);
            data.push(comp.len() as u8);
            data.extend_from_slice(comp.as_bytes());
        }
        let rr = parse(&entry(b"SL", &data), &mut NoReader);
        assert_eq!(rr.symlink_target.as_deref(), Some("/etc/hosts"));
    }

    #[test]
    fn parse_ce_continuation_is_followed() {
        // Place a NM entry in a continuation area at sector 5, offset 0.
        let mut img = vec![0u8; 6 * SECTOR_SIZE as usize];
        let nm = {
            let mut d = vec![0u8];
            d.extend_from_slice(b"continued.txt");
            entry(b"NM", &d)
        };
        let ce_sector = 5u32;
        img[(ce_sector as usize) * SECTOR_SIZE as usize
            ..(ce_sector as usize) * SECTOR_SIZE as usize + nm.len()]
            .copy_from_slice(&nm);

        // Primary System Use area holds only a CE pointing at sector 5.
        let mut ce_data = Vec::new();
        ce_data.extend(both(ce_sector)); // block
        ce_data.extend(both(0)); // offset
        ce_data.extend(both(nm.len() as u32)); // length
        let su = entry(b"CE", &ce_data);

        let rr = parse(&su, &mut MemReader(img));
        assert_eq!(rr.name.as_deref(), Some("continued.txt"));
    }
}
