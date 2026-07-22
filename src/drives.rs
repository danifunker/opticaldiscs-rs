//! Physical optical drive enumeration.
//!
//! Requires feature `drives`. Provides [`OpticalDrive`] and [`list_drives`].
//!
//! ## Platform notes
//!
//! - **Linux**: Scans `/sys/block/sr*` for `sr`-class block devices and reads
//!   the drive model from `/sys/block/srN/device/{vendor,model}`.
//! - **macOS**: Runs `ioreg -r -c <class> -l -w 0` over each optical-drive
//!   class (`IO{CD,DVD,BD}Services` for modern, notably USB-attached, drives
//!   plus the legacy `IO{CD,DVD,BD}DriveNub`) and parses `Vendor Name`,
//!   `Product Name`, `BSD Name`, and `Media Present` out of each subtree.
//!   A drive with an empty tray has no `/dev` node at all, so it is reported
//!   with an empty `device_path` and `is_loaded == false`.
//! - **Windows**: Enumerates drive letters A–Z with `GetDriveTypeW` and
//!   identifies `DRIVE_CDROM` entries; uses `GetVolumeInformationW` to
//!   obtain the disc volume label and confirm that media is present.
//! - **Other platforms**: Returns an empty `Vec`.
//!
//! See PLAN.md Phase 9 for implementation details.

use std::path::PathBuf;

// ── Public types ──────────────────────────────────────────────────────────────

/// A physical optical drive (CD, DVD, or Blu-ray) detected on the current system.
#[derive(Debug, Clone)]
pub struct OpticalDrive {
    /// Path to the device node.
    ///
    /// Examples: `/dev/sr0` (Linux), `/dev/disk2` (macOS), `D:\` (Windows).
    pub device_path: PathBuf,
    /// Human-readable drive or disc name.
    ///
    /// On Linux and macOS this is the drive model (e.g. `"SAMSUNG SH-224FB"`).
    /// On Windows this is the disc volume label (e.g. `"GAME_DISC (D:)"`) when
    /// media is loaded, or `"CD/DVD Drive (D:)"` when the tray is empty.
    pub display_name: String,
    /// Whether optical media is currently loaded in the drive.
    pub is_loaded: bool,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Return all optical drives detected on the current system.
///
/// Returns an empty `Vec` on unsupported platforms or when no drives are
/// found.  The function never panics; any OS error is silently ignored and
/// results in a shorter (or empty) list.
pub fn list_drives() -> Vec<OpticalDrive> {
    imp::list()
}

// ── Linux ─────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod imp {
    use super::OpticalDrive;
    use std::path::{Path, PathBuf};

    pub fn list() -> Vec<OpticalDrive> {
        let Ok(block_dir) = std::fs::read_dir("/sys/block") else {
            return Vec::new();
        };

        let mut drives: Vec<OpticalDrive> = block_dir
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                // Only sr* devices are optical drives.
                if !name_str.starts_with("sr") {
                    return None;
                }

                let sys_path = entry.path();
                let vendor = read_sys_str(&sys_path.join("device/vendor")).unwrap_or_default();
                let model = read_sys_str(&sys_path.join("device/model")).unwrap_or_default();
                let display_name = format!("{} {}", vendor.trim(), model.trim())
                    .trim()
                    .to_string();

                // /sys/block/srN/size is 0 when no disc is present.
                let size_str = read_sys_str(&sys_path.join("size")).unwrap_or_default();
                let is_loaded = size_str.trim().parse::<u64>().unwrap_or(0) > 0;

                Some(OpticalDrive {
                    device_path: PathBuf::from(format!("/dev/{name_str}")),
                    display_name,
                    is_loaded,
                })
            })
            .collect();

        drives.sort_by(|a, b| a.device_path.cmp(&b.device_path));
        drives
    }

    fn read_sys_str(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

// ── macOS ─────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod imp {
    use super::OpticalDrive;
    use std::path::PathBuf;
    use std::process::Command;

    /// IOKit classes that stand for a physical optical drive.
    ///
    /// Modern macOS publishes optical drives — notably every USB-attached one —
    /// as `IO{CD,DVD,BD}Services`. The legacy `IO*DriveNub` classes are queried
    /// too so internal ATAPI drives on vintage systems keep working. A single
    /// drive usually answers to more than one of these; [`list`] de-duplicates
    /// on the registry object id.
    pub(super) const DRIVE_CLASSES: &[&str] = &[
        "IOCDServices",
        "IODVDServices",
        "IOBDServices",
        "IOCDDriveNub",
        "IODVDDriveNub",
        "IOBDDriveNub",
    ];

    pub fn list() -> Vec<OpticalDrive> {
        let mut drives: Vec<OpticalDrive> = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        for class in DRIVE_CLASSES {
            // `-w 0` disables ioreg's line wrapping: the drive's identity lives
            // in a single-line "Device Characteristics" dict that wrapping would
            // otherwise split mid-value.
            let Ok(output) = Command::new("ioreg")
                .args(["-r", "-c", class, "-l", "-w", "0"])
                .output()
            else {
                continue;
            };

            let stdout = String::from_utf8_lossy(&output.stdout);
            for (id, drive) in parse_ioreg_entries(&stdout) {
                match id {
                    Some(id) if seen.contains(&id) => continue,
                    Some(id) => seen.push(id),
                    None => {}
                }
                drives.push(drive);
            }
        }

        drives.sort_by(|a, b| a.device_path.cmp(&b.device_path));
        drives
    }

    /// Parse `ioreg -r -c <class> -l -w 0` output into a list of drives.
    pub(super) fn parse_ioreg_output(text: &str) -> Vec<OpticalDrive> {
        parse_ioreg_entries(text)
            .into_iter()
            .map(|(_, d)| d)
            .collect()
    }

    /// Parse ioreg output into `(registry object id, drive)` pairs.
    ///
    /// `-r` prints one subtree per matched object: the root line starts at
    /// column 0 and everything indented under it belongs to that drive. The
    /// disc's `BSD Name` lives on an `IOMedia` node nested inside the subtree,
    /// so the whole subtree — not just the root's own property block — is
    /// scanned.
    fn parse_ioreg_entries(text: &str) -> Vec<(Option<String>, OpticalDrive)> {
        let mut drives = Vec::new();

        for subtree in split_subtrees(text) {
            let header = subtree.lines().next().unwrap_or_default();
            // Only accept subtrees rooted at a real optical-drive class; the
            // caller may have piped in unrelated objects.
            let class = header.trim_start().trim_start_matches("+-o ").trim_start();
            if !DRIVE_CLASSES.iter().any(|c| class.starts_with(c)) {
                continue;
            }

            let mut vendor = String::new();
            let mut product = String::new();
            let mut bsd_name = String::new();
            let mut media_present: Option<bool> = None;

            for line in subtree.lines() {
                let line = line.trim();
                // "Vendor Name" / "Product Name" appear both as standalone
                // properties (legacy nubs) and inside the one-line "Device
                // Characteristics" dict (modern services); parse_str_value
                // handles either spelling.
                if let Some(v) = parse_str_value(line, "Vendor Name") {
                    vendor = v;
                }
                if let Some(v) = parse_str_value(line, "Product Name") {
                    product = v;
                }
                // Take the whole-disc node (`disk6`), never a partition
                // (`disk6s1`) — a hybrid disc publishes several of those.
                if bsd_name.is_empty() {
                    if let Some(v) = parse_str_value(line, "BSD Name") {
                        if is_whole_disk(&v) {
                            bsd_name = v;
                        }
                    }
                }
                if line.contains("\"Media Present\"") {
                    // Boolean value: `"Media Present" = Yes` or `=No`.
                    media_present = Some(line.replace(' ', "").contains("=Yes"));
                }
            }

            // An empty tray has no /dev node at all on macOS, so a missing BSD
            // name means "no disc" — not "not a drive". The drive is still
            // listed (matching the Linux behaviour of listing an empty sr0) so
            // the user can see it is connected and insert a disc.
            let is_loaded = media_present.unwrap_or(!bsd_name.is_empty());

            // Combine Vendor and Product Name, trimming interior whitespace.
            let display_name = {
                let v = vendor.trim();
                let p = product.trim();
                match (v, p) {
                    ("", "") if !bsd_name.is_empty() => bsd_name.clone(),
                    ("", "") => "Optical Drive".to_string(),
                    ("", p) => p.to_string(),
                    (v, "") => v.to_string(),
                    (v, p) => format!("{v} {p}"),
                }
            };

            let device_path = if bsd_name.is_empty() {
                PathBuf::new()
            } else {
                PathBuf::from(format!("/dev/{bsd_name}"))
            };

            drives.push((
                parse_object_id(header),
                OpticalDrive {
                    device_path,
                    display_name,
                    is_loaded,
                },
            ));
        }

        drives
    }

    /// Split ioreg output into one slice per top-level `+-o` subtree.
    fn split_subtrees(text: &str) -> Vec<&str> {
        let mut starts: Vec<usize> = Vec::new();
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            // Only a subtree root is flush against column 0; children are
            // indented by the tree-drawing prefix.
            if line.starts_with("+-o") {
                starts.push(offset);
            }
            offset += line.len();
        }

        starts
            .iter()
            .enumerate()
            .map(|(i, &start)| {
                let end = starts.get(i + 1).copied().unwrap_or(text.len());
                &text[start..end]
            })
            .collect()
    }

    /// Pull the registry object id out of a `+-o` header line, e.g.
    /// `+-o IODVDServices  <class IODVDServices, id 0x1000bccd4, ...>`.
    fn parse_object_id(header: &str) -> Option<String> {
        let start = header.find(", id ")? + ", id ".len();
        let rest = &header[start..];
        let end = rest.find(&[',', '>'][..])?;
        Some(rest[..end].trim().to_string())
    }

    /// Is this a whole-disk BSD name (`disk6`) rather than a partition
    /// (`disk6s1`)?
    fn is_whole_disk(name: &str) -> bool {
        match name.strip_prefix("disk") {
            Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
            None => false,
        }
    }

    /// Parse a `"Key" = "value"` (or dict-packed `"Key"="value"`) pair out of a
    /// line and return `"value"`.
    pub(super) fn parse_str_value(line: &str, key: &str) -> Option<String> {
        let key_pat = format!("\"{key}\"");
        let after_key = &line[line.find(key_pat.as_str())? + key_pat.len()..];
        let after_eq = after_key.trim_start().strip_prefix('=')?.trim_start();
        let rest = after_eq.strip_prefix('"')?;
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    }
}

// ── Windows ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod imp {
    use super::OpticalDrive;
    use std::path::PathBuf;

    /// `GetDriveTypeW` return value for CD-ROM drives.
    const DRIVE_CDROM: u32 = 5;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDriveTypeW(lpRootPathName: *const u16) -> u32;
        fn GetVolumeInformationW(
            lpRootPathName: *const u16,
            lpVolumeNameBuffer: *mut u16,
            nVolumeNameSize: u32,
            lpVolumeSerialNumber: *mut u32,
            lpMaximumComponentLength: *mut u32,
            lpFileSystemFlags: *mut u32,
            lpFileSystemNameBuffer: *mut u16,
            nFileSystemNameSize: u32,
        ) -> i32;
    }

    pub fn list() -> Vec<OpticalDrive> {
        let mut drives = Vec::new();

        for letter in b'A'..=b'Z' {
            let letter_char = letter as char;
            let root = format!("{letter_char}:\\");

            // Encode as null-terminated UTF-16.
            let root_w: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();

            let drive_type = unsafe { GetDriveTypeW(root_w.as_ptr()) };
            if drive_type != DRIVE_CDROM {
                continue;
            }

            // Try to retrieve the volume label; success means media is loaded.
            let mut vol_name = [0u16; 256];
            let ok = unsafe {
                GetVolumeInformationW(
                    root_w.as_ptr(),
                    vol_name.as_mut_ptr(),
                    256,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                )
            };

            let is_loaded = ok != 0;
            let display_name = if is_loaded {
                let end = vol_name
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(vol_name.len());
                let label = String::from_utf16_lossy(&vol_name[..end]);
                if label.is_empty() {
                    format!("CD/DVD Drive ({letter_char}:)")
                } else {
                    format!("{label} ({letter_char}:)")
                }
            } else {
                format!("CD/DVD Drive ({letter_char}:)")
            };

            drives.push(OpticalDrive {
                device_path: PathBuf::from(&root),
                display_name,
                is_loaded,
            });
        }

        drives
    }
}

// ── Other platforms ───────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    use super::OpticalDrive;

    pub fn list() -> Vec<OpticalDrive> {
        Vec::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optical_drive_struct_fields() {
        let d = OpticalDrive {
            device_path: "/dev/sr0".into(),
            display_name: "SAMSUNG SH-224FB".into(),
            is_loaded: true,
        };
        assert_eq!(d.device_path.to_string_lossy(), "/dev/sr0");
        assert_eq!(d.display_name, "SAMSUNG SH-224FB");
        assert!(d.is_loaded);
    }

    #[test]
    fn optical_drive_not_loaded() {
        let d = OpticalDrive {
            device_path: "/dev/sr0".into(),
            display_name: "PIONEER BD-RW BDR-209".into(),
            is_loaded: false,
        };
        assert!(!d.is_loaded);
    }

    /// Smoke test: `list_drives()` must not panic and returns a `Vec`.
    ///
    /// This is the "9.6 manual / integration test": it runs on real hardware
    /// and logs whatever drives are found without asserting a specific count.
    #[test]
    fn list_drives_smoke_test() {
        let drives = list_drives();
        println!("Found {} optical drive(s):", drives.len());
        for d in &drives {
            println!(
                "  {} — {} [loaded={}]",
                d.device_path.display(),
                d.display_name,
                d.is_loaded
            );
        }
        // No assertion: the test machine may have no optical drives.
    }

    // ── macOS-specific unit tests ─────────────────────────────────────────────

    #[cfg(target_os = "macos")]
    mod macos {
        use super::super::imp;

        #[test]
        fn parse_str_value_extracts_quoted_string() {
            let line = r#"    "Product Name" = "DVDRAM GH22NS90""#;
            assert_eq!(
                imp::parse_str_value(line, "Product Name").as_deref(),
                Some("DVDRAM GH22NS90")
            );
        }

        #[test]
        fn parse_str_value_wrong_key_returns_none() {
            let line = r#"    "Product Name" = "DVDRAM GH22NS90""#;
            assert!(imp::parse_str_value(line, "Vendor Name").is_none());
        }

        #[test]
        fn parse_ioreg_empty_input() {
            assert!(imp::parse_ioreg_output("").is_empty());
        }

        #[test]
        fn parse_ioreg_single_drive_loaded() {
            let text = concat!(
                "+-o IODVDDriveNub  <class IODVDDriveNub>\n",
                "  {\n",
                "    \"Vendor Name\" = \"LG\"\n",
                "    \"Product Name\" = \"DVDRAM GH22NS90\"\n",
                "    \"BSD Name\" = \"disk5\"\n",
                "    \"Media Present\" = Yes\n",
                "  }\n"
            );
            let drives = imp::parse_ioreg_output(text);
            assert_eq!(drives.len(), 1);
            assert_eq!(drives[0].display_name, "LG DVDRAM GH22NS90");
            assert_eq!(drives[0].device_path.to_str().unwrap(), "/dev/disk5");
            assert!(drives[0].is_loaded);
        }

        #[test]
        fn parse_ioreg_single_drive_no_media() {
            let text = concat!(
                "+-o IODVDDriveNub  <class IODVDDriveNub>\n",
                "  {\n",
                "    \"Product Name\" = \"DVDRAM GH22NS90\"\n",
                "    \"BSD Name\" = \"disk5\"\n",
                "    \"Media Present\" = No\n",
                "  }\n"
            );
            let drives = imp::parse_ioreg_output(text);
            assert_eq!(drives.len(), 1);
            assert!(!drives[0].is_loaded);
            assert_eq!(drives[0].display_name, "DVDRAM GH22NS90");
        }

        #[test]
        fn parse_ioreg_vendor_only() {
            let text = concat!(
                "+-o IODVDDriveNub  <class IODVDDriveNub>\n",
                "  {\n",
                "    \"Vendor Name\" = \"PIONEER\"\n",
                "    \"BSD Name\" = \"disk3\"\n",
                "    \"Media Present\" = No\n",
                "  }\n"
            );
            let drives = imp::parse_ioreg_output(text);
            assert_eq!(drives[0].display_name, "PIONEER");
        }

        #[test]
        fn parse_ioreg_skips_non_dvd_nub() {
            // A block that doesn't start with IODVDDriveNub should be skipped.
            let text = concat!(
                "+-o IOBlockStorageDevice  <class IOBlockStorageDevice>\n",
                "  {\n",
                "    \"BSD Name\" = \"disk2\"\n",
                "  }\n",
                "+-o IODVDDriveNub  <class IODVDDriveNub>\n",
                "  {\n",
                "    \"BSD Name\" = \"disk4\"\n",
                "    \"Media Present\" = No\n",
                "  }\n"
            );
            let drives = imp::parse_ioreg_output(text);
            assert_eq!(drives.len(), 1);
            assert_eq!(drives[0].device_path.to_str().unwrap(), "/dev/disk4");
        }

        #[test]
        fn parse_ioreg_missing_bsd_name_still_lists_the_drive() {
            let text = concat!(
                "+-o IODVDDriveNub  <class IODVDDriveNub>\n",
                "  {\n",
                "    \"Product Name\" = \"DVDRAM GH22NS90\"\n",
                "    \"Media Present\" = No\n",
                "  }\n"
            );
            // An empty tray has no /dev node on macOS. The drive is connected,
            // so it is listed with no path rather than hidden.
            let drives = imp::parse_ioreg_output(text);
            assert_eq!(drives.len(), 1);
            assert_eq!(drives[0].display_name, "DVDRAM GH22NS90");
            assert_eq!(drives[0].device_path.as_os_str(), "");
            assert!(!drives[0].is_loaded);
        }

        /// Verbatim `ioreg -r -c IODVDServices -l -w 0` output for a USB
        /// HL-DT-ST GP65NW60, the shape that `IODVDDriveNub`-only enumeration
        /// missed entirely.
        const USB_DVD_SERVICES: &str = concat!(
            "+-o IODVDServices  <class IODVDServices, id 0x1000bccd4, registered, matched, active, busy 0 (374 ms), retain 8>\n",
            "  | {\n",
            "  |   \"device-type\" = \"DVD\"\n",
            "  |   \"Protocol Characteristics\" = {\"Physical Interconnect\"=\"USB\",\"SCSI Logical Unit Number\"=0,\"Physical Interconnect Location\"=\"External\"}\n",
            "  |   \"Device Characteristics\" = {\"Product Name\"=\"DVDRAM GP65NW60\",\"Power Off\"=No,\"CD Features\"=2047,\"Vendor Name\"=\"HL-DT-ST\",\"Loading Mechanism\"=\"Tray\",\"Product Revision Level\"=\"RF01\"}\n",
            "  | }\n",
            "  | \n",
            "  +-o IODVDBlockStorageDriver  <class IODVDBlockStorageDriver, id 0x1000bccd6, registered, matched, active, busy 0, retain 7>\n",
            "    +-o IODVDMedia  <class IODVDMedia, id 0x1000bccd9, registered, matched, active, busy 0, retain 6>\n",
            "      | {\n",
            "      |   \"Whole\" = Yes\n",
            "      |   \"BSD Name\" = \"disk6\"\n",
            "      |   \"Ejectable\" = Yes\n",
            "      | }\n",
            "      +-o IOMediaBSDClient  <class IOMediaBSDClient, id 0x1000bccdb, registered, matched, active, busy 0, retain 4>\n",
        );

        #[test]
        fn parse_ioreg_usb_dvd_services_with_disc() {
            let drives = imp::parse_ioreg_output(USB_DVD_SERVICES);
            assert_eq!(drives.len(), 1);
            // Identity comes out of the single-line Device Characteristics dict.
            assert_eq!(drives[0].display_name, "HL-DT-ST DVDRAM GP65NW60");
            // BSD Name comes from the nested IODVDMedia node, not the root.
            assert_eq!(drives[0].device_path.to_str().unwrap(), "/dev/disk6");
            assert!(drives[0].is_loaded);
        }

        #[test]
        fn parse_ioreg_usb_dvd_services_empty_tray() {
            // Same drive, disc ejected: the IODVDBlockStorageDriver has no
            // media child at all.
            let text = concat!(
                "+-o IODVDServices  <class IODVDServices, id 0x1000bccd4, registered, matched, active, busy 0, retain 8>\n",
                "  | {\n",
                "  |   \"Device Characteristics\" = {\"Product Name\"=\"DVDRAM GP65NW60\",\"Vendor Name\"=\"HL-DT-ST\"}\n",
                "  | }\n",
                "  +-o IODVDBlockStorageDriver  <class IODVDBlockStorageDriver, id 0x1000bccd6, registered, matched, active, busy 0, retain 7>\n",
            );
            let drives = imp::parse_ioreg_output(text);
            assert_eq!(drives.len(), 1);
            assert_eq!(drives[0].display_name, "HL-DT-ST DVDRAM GP65NW60");
            assert_eq!(drives[0].device_path.as_os_str(), "");
            assert!(!drives[0].is_loaded);
        }

        #[test]
        fn parse_ioreg_ignores_partition_bsd_names() {
            // A hybrid ISO9660/HFS disc publishes disk6s1, disk6s1s2, ... The
            // whole-disc node is the only correct rip target.
            let text = concat!(
                "+-o IOCDServices  <class IOCDServices, id 0x100000001, registered>\n",
                "  +-o IOCDMedia  <class IOCDMedia, id 0x100000002, registered>\n",
                "      |   \"BSD Name\" = \"disk6s1\"\n",
                "      |   \"Whole\" = No\n",
                "  +-o IOCDMedia  <class IOCDMedia, id 0x100000003, registered>\n",
                "      |   \"BSD Name\" = \"disk6\"\n",
                "      |   \"Whole\" = Yes\n",
            );
            let drives = imp::parse_ioreg_output(text);
            assert_eq!(drives.len(), 1);
            assert_eq!(drives[0].device_path.to_str().unwrap(), "/dev/disk6");
        }

        #[test]
        fn parse_ioreg_separates_two_subtrees() {
            let text = format!("{USB_DVD_SERVICES}{USB_DVD_SERVICES}");
            // Two roots parse as two drives (list() de-duplicates on object id).
            assert_eq!(imp::parse_ioreg_output(&text).len(), 2);
        }

        #[test]
        fn parse_str_value_handles_dict_packed_pairs() {
            let line = r#"  |   "Device Characteristics" = {"Product Name"="GP65NW60","Vendor Name"="HL-DT-ST"}"#;
            assert_eq!(
                imp::parse_str_value(line, "Vendor Name").as_deref(),
                Some("HL-DT-ST")
            );
            assert_eq!(
                imp::parse_str_value(line, "Product Name").as_deref(),
                Some("GP65NW60")
            );
        }
    }
}
