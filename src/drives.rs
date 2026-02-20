//! Physical optical drive enumeration.
//!
//! Requires feature `drives`. Provides [`OpticalDrive`] and [`list_drives`].
//!
//! ## Platform notes
//!
//! - **Linux**: Scans `/sys/block/sr*` for `sr`-class block devices and reads
//!   the drive model from `/sys/block/srN/device/{vendor,model}`.
//! - **macOS**: Runs `ioreg -r -c IODVDDriveNub -l` to enumerate DVD drive
//!   nubs and parses their `BSD Name`, `Vendor Name`, `Product Name`, and
//!   `Media Present` fields.
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

    pub fn list() -> Vec<OpticalDrive> {
        let output = match Command::new("ioreg")
            .args(["-r", "-c", "IODVDDriveNub", "-l"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_ioreg_output(&stdout)
    }

    /// Parse the output of `ioreg -r -c IODVDDriveNub -l` into a list of drives.
    pub(super) fn parse_ioreg_output(text: &str) -> Vec<OpticalDrive> {
        // Each device block starts with a "+-o" line; split on that delimiter.
        let mut drives = Vec::new();

        for block in text.split("+-o").skip(1) {
            // Only handle IODVDDriveNub entries.
            if !block.trim_start().starts_with("IODVDDriveNub") {
                continue;
            }

            let mut vendor = String::new();
            let mut product = String::new();
            let mut bsd_name = String::new();
            let mut is_loaded = false;

            for line in block.lines() {
                let line = line.trim();
                if let Some(v) = parse_str_value(line, "Vendor Name") {
                    vendor = v;
                } else if let Some(v) = parse_str_value(line, "Product Name") {
                    product = v;
                } else if let Some(v) = parse_str_value(line, "BSD Name") {
                    bsd_name = v;
                } else if line.contains("\"Media Present\"") {
                    // Boolean value: `"Media Present" = Yes` or `= No`
                    is_loaded = line.contains("= Yes");
                }
            }

            if bsd_name.is_empty() {
                continue;
            }

            // Combine Vendor and Product Name, trimming interior whitespace.
            let display_name = {
                let v = vendor.trim();
                let p = product.trim();
                match (v, p) {
                    ("", p) if !p.is_empty() => p.to_string(),
                    (v, "") if !v.is_empty() => v.to_string(),
                    (v, p) if !v.is_empty() => format!("{v} {p}"),
                    _ => bsd_name.clone(),
                }
            };

            drives.push(OpticalDrive {
                device_path: PathBuf::from(format!("/dev/{bsd_name}")),
                display_name,
                is_loaded,
            });
        }

        drives
    }

    /// Parse a line of the form `"Key" = "value"` and return `"value"`.
    pub(super) fn parse_str_value(line: &str, key: &str) -> Option<String> {
        let prefix = format!("\"{key}\" = \"");
        let start = line.find(prefix.as_str())? + prefix.len();
        let rest = &line[start..];
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
        fn parse_ioreg_missing_bsd_name_skipped() {
            let text = concat!(
                "+-o IODVDDriveNub  <class IODVDDriveNub>\n",
                "  {\n",
                "    \"Product Name\" = \"DVDRAM GH22NS90\"\n",
                "    \"Media Present\" = No\n",
                "  }\n"
            );
            // No BSD Name → should be skipped.
            let drives = imp::parse_ioreg_output(text);
            assert!(drives.is_empty());
        }
    }
}
