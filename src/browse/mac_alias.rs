//! Classic Mac alias resolution.
//!
//! Parses the `alis` resource record stored in a file's resource fork
//! (the marker for aliases is `kIsAlias`/`0x8000` in `FInfo.fdFlags`).
//!
//! We extract just enough to show the user where an alias points:
//!   - Target filename (from the fixed portion at offset 50)
//!   - Volume name   (from the fixed portion at offset 10)
//!   - Optional absolute path from tagged record 18 (UTF-8) or 2 (MacRoman)
//!
//! We do not attempt to follow the alias; the target may no longer exist.

/// Finder flag bit indicating the file is an alias.
pub const IS_ALIAS_FLAG: u16 = 0x8000;

/// Type code for classic HFS+ UNIX symlinks (raw Finder type bytes).
pub const SLNK_TYPE: [u8; 4] = *b"slnk";
/// Creator code for classic HFS+ UNIX symlinks (raw Finder creator bytes).
pub const RHAP_CREATOR: [u8; 4] = *b"rhap";

fn read_u16_be(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn read_u32_be(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Read a Pascal string padded to `field_len` bytes.
fn read_pascal_string(field: &[u8]) -> String {
    if field.is_empty() {
        return String::new();
    }
    let len = field[0] as usize;
    let max = field.len().saturating_sub(1).min(len);
    let mut out = String::with_capacity(max);
    for &b in &field[1..1 + max] {
        if b < 0x80 {
            out.push(b as char);
        } else {
            out.push('?');
        }
    }
    out
}

/// Parse an `alis` resource record and return a human-readable target string.
pub fn parse_alis_record(data: &[u8]) -> Option<String> {
    if data.len() < 114 {
        return None;
    }
    let volume = read_pascal_string(&data[10..38]);
    let target = read_pascal_string(&data[50..114]);

    let abs_path = if data.len() > 150 {
        scan_tagged_path(&data[150..])
    } else {
        None
    };

    match (abs_path, volume.is_empty(), target.is_empty()) {
        (Some(p), _, _) if !p.is_empty() => Some(p),
        (_, true, true) => None,
        (_, true, false) => Some(target),
        (_, false, true) => Some(format!("{volume}:")),
        (_, false, false) => Some(format!("{volume}:{target}")),
    }
}

/// Walk tag-length-value records after the fixed portion looking for a path.
fn scan_tagged_path(tags: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 4 <= tags.len() {
        let tag = read_u16_be(tags, i);
        let len = read_u16_be(tags, i + 2) as usize;
        if tag == 0xFFFF {
            return None;
        }
        let data_start = i + 4;
        let data_end = data_start.checked_add(len)?;
        if data_end > tags.len() {
            return None;
        }
        match tag {
            18 => {
                if let Ok(s) = std::str::from_utf8(&tags[data_start..data_end]) {
                    let trimmed = s.trim_end_matches('\0');
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            2 => {
                let mut out = String::with_capacity(len);
                for &b in &tags[data_start..data_end] {
                    if b == 0 {
                        break;
                    } else if b < 0x80 {
                        out.push(b as char);
                    } else {
                        out.push('?');
                    }
                }
                if !out.is_empty() {
                    return Some(out);
                }
            }
            _ => {}
        }
        let padded = (len + 1) & !1;
        i = data_start + padded;
    }
    None
}

/// Parse a resource fork and return the data of the first `alis` resource.
///
/// Map header layout (28 bytes fixed):
///   [0..16]  copy of rsrc header
///   [16..20] handle to next map (reserved)
///   [20..22] file reference number (reserved)
///   [22..24] fork attrs
///   [24..26] offset from map beginning to type list
///   [26..28] offset from map beginning to name list
pub fn find_alis_in_resource_fork(rsrc: &[u8]) -> Option<Vec<u8>> {
    if rsrc.len() < 16 {
        return None;
    }
    let data_off = read_u32_be(rsrc, 0) as usize;
    let map_off = read_u32_be(rsrc, 4) as usize;
    let map_len = read_u32_be(rsrc, 12) as usize;

    if map_off + map_len > rsrc.len() || map_off + 30 > rsrc.len() {
        return None;
    }
    let map = &rsrc[map_off..map_off + map_len.min(rsrc.len() - map_off)];
    if map.len() < 30 {
        return None;
    }

    let type_list_off = read_u16_be(map, 24) as usize;
    if type_list_off + 2 > map.len() {
        return None;
    }

    let num_types = read_u16_be(map, type_list_off).wrapping_add(1);
    let types_start = type_list_off + 2;

    for t in 0..num_types as usize {
        let te = types_start + t * 8;
        if te + 8 > map.len() {
            return None;
        }
        let four_cc = &map[te..te + 4];
        let count = read_u16_be(map, te + 4).wrapping_add(1);
        let ref_list_off = read_u16_be(map, te + 6) as usize;

        if four_cc == b"alis" && count > 0 {
            let re = type_list_off + ref_list_off;
            if re + 12 > map.len() {
                return None;
            }
            // Data offset: 24-bit value at bytes 5..8 (byte 4 is attrs).
            let data_rel = ((map[re + 5] as usize) << 16)
                | ((map[re + 6] as usize) << 8)
                | (map[re + 7] as usize);
            let abs = data_off + data_rel;
            if abs + 4 > rsrc.len() {
                return None;
            }
            let res_len = read_u32_be(rsrc, abs) as usize;
            let res_start = abs + 4;
            if res_start + res_len > rsrc.len() {
                return None;
            }
            return Some(rsrc[res_start..res_start + res_len].to_vec());
        }
    }
    None
}

/// One-shot: given rsrc fork bytes, return a display string for the alias target.
pub fn resolve_alias_target(rsrc: &[u8]) -> Option<String> {
    let alis = find_alis_in_resource_fork(rsrc)?;
    parse_alis_record(&alis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pascal_field(s: &str, field_len: usize) -> Vec<u8> {
        let mut out = vec![0u8; field_len];
        let bytes = s.as_bytes();
        let n = bytes.len().min(field_len - 1).min(u8::MAX as usize);
        out[0] = n as u8;
        out[1..1 + n].copy_from_slice(&bytes[..n]);
        out
    }

    fn build_alis_record(volume: &str, target: &str, extra_tags: &[u8]) -> Vec<u8> {
        let mut rec = vec![0u8; 150];
        rec[10..38].copy_from_slice(&build_pascal_field(volume, 28));
        rec[50..114].copy_from_slice(&build_pascal_field(target, 64));
        rec.extend_from_slice(extra_tags);
        rec
    }

    #[test]
    fn parse_alis_minimal_volume_and_target() {
        let rec = build_alis_record("MyDisk", "ReadMe", &[]);
        assert_eq!(parse_alis_record(&rec).as_deref(), Some("MyDisk:ReadMe"));
    }

    #[test]
    fn parse_alis_tag_18_utf8_path_wins() {
        let mut tags = Vec::new();
        tags.extend_from_slice(&18u16.to_be_bytes());
        let path = b"/Volumes/MyDisk/Apps/MyApp.app";
        tags.extend_from_slice(&(path.len() as u16).to_be_bytes());
        tags.extend_from_slice(path);
        if !path.len().is_multiple_of(2) {
            tags.push(0);
        }
        tags.extend_from_slice(&0xFFFFu16.to_be_bytes());
        tags.extend_from_slice(&0u16.to_be_bytes());
        let rec = build_alis_record("MyDisk", "MyApp.app", &tags);
        assert_eq!(
            parse_alis_record(&rec).as_deref(),
            Some("/Volumes/MyDisk/Apps/MyApp.app")
        );
    }

    #[test]
    fn parse_alis_rejects_short() {
        assert!(parse_alis_record(&[0u8; 50]).is_none());
    }

    #[test]
    fn parse_alis_empty_volume_and_target() {
        let rec = build_alis_record("", "", &[]);
        assert!(parse_alis_record(&rec).is_none());
    }

    #[test]
    fn find_alis_in_minimal_rsrc() {
        let alis_body = b"hello-alias";
        let mut buf = vec![0u8; 16];
        buf.extend_from_slice(&(alis_body.len() as u32).to_be_bytes());
        buf.extend_from_slice(alis_body);
        let map_off = buf.len();

        let map_header_size = 28;
        let type_list_off_in_map = map_header_size;
        let ref_list_off_from_type_list = 2 + 8;

        let mut map = vec![0u8; map_header_size];
        map[24] = (type_list_off_in_map >> 8) as u8;
        map[25] = (type_list_off_in_map & 0xFF) as u8;
        map[26] = 0xFF;
        map[27] = 0xFF;
        map.extend_from_slice(&[0, 0]); // num_types - 1
        map.extend_from_slice(b"alis");
        map.extend_from_slice(&[0, 0]); // count - 1
        map.extend_from_slice(&(ref_list_off_from_type_list as u16).to_be_bytes());
        map.extend_from_slice(&[0, 0]); // id
        map.extend_from_slice(&[0xFF, 0xFF]); // name off
        map.push(0); // attrs byte
        map.extend_from_slice(&[0, 0, 0]); // 24-bit data offset = 0
        map.extend_from_slice(&[0, 0, 0, 0]); // reserved

        let map_len = map.len();
        buf[0..4].copy_from_slice(&16u32.to_be_bytes());
        buf[4..8].copy_from_slice(&(map_off as u32).to_be_bytes());
        buf[8..12].copy_from_slice(&((4 + alis_body.len()) as u32).to_be_bytes());
        buf[12..16].copy_from_slice(&(map_len as u32).to_be_bytes());
        buf.extend_from_slice(&map);

        let found = find_alis_in_resource_fork(&buf).expect("alis should be found");
        assert_eq!(found, alis_body);
    }
}
