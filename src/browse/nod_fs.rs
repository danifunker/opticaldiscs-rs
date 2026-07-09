//! Nintendo GameCube / Wii filesystem browser, backed by the [`nod`] crate.
//!
//! [`NodeFilesystem`] adapts `nod`'s disc/partition/FST API onto this crate's
//! [`Filesystem`] trait. `nod` handles every hard part — the GameCube FST, the
//! Wii partition table, AES decryption of Wii partition data (the common keys
//! are compiled into `nod`, so no user-supplied key file is required), and the
//! compressed container formats (RVZ/WIA/WBFS/CISO/GCZ/TGC/NFS). We only map its
//! `Node`s to [`FileEntry`]s and its file streams to byte reads.
//!
//! Reference: `docs/GameDiscs_Implementation.md` (Phase G2).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use nod::{Disc, PartitionBase, PartitionKind, PartitionMeta};

use super::entry::{EntryType, FileEntry};
use super::filesystem::{Filesystem, FilesystemError};
use crate::formats::FilesystemType;
use crate::gameid::{Console, GameDiscInfo, Region};

/// A browsable GameCube/Wii filesystem.
///
/// Holds an open `nod` partition and its metadata; the FST is re-derived from
/// the owned metadata on each call (cheap — it is a view over an in-memory
/// buffer). Each [`FileEntry`]'s `location` field carries the FST node index,
/// so directory listing and file reads resolve back to the right `nod::Node`.
pub struct NodeFilesystem {
    partition: Box<dyn PartitionBase>,
    meta: Box<PartitionMeta>,
    volume_name: String,
}

impl NodeFilesystem {
    /// Open the data partition of the GameCube/Wii disc image at `path`.
    pub fn new(path: &Path) -> Result<Self, FilesystemError> {
        // NKit-processed images preserve the disc header (so they *identify* as a
        // normal GC/Wii disc) but scrub and rearrange the partition data. `nod`
        // cannot reconstruct that and stalls trying to read it, so refuse to
        // browse rather than hang — the image must be converted to a full
        // ISO/RVZ first.
        if is_nkit_image(path) {
            return Err(FilesystemError::Unsupported);
        }
        let disc = Disc::new(path).map_err(nod_err)?;
        let volume_name = ascii_z(&disc.header().game_title);
        let mut partition = disc
            .open_partition_kind(PartitionKind::Data)
            .map_err(nod_err)?;
        let meta = partition.meta().map_err(nod_err)?;
        Ok(Self {
            partition,
            meta,
            volume_name,
        })
    }

    /// Look up the `nod::Node` at FST index `idx` in the owned metadata.
    fn node_at(&self, idx: usize) -> Result<nod::Node, FilesystemError> {
        let fst = self
            .meta
            .fst()
            .map_err(|e| FilesystemError::Parse(e.into()))?;
        fst.nodes
            .get(idx)
            .copied()
            .ok_or_else(|| FilesystemError::NotFound(format!("FST node {idx}")))
    }
}

impl Filesystem for NodeFilesystem {
    fn root(&mut self) -> Result<FileEntry, FilesystemError> {
        // Root is always FST node 0.
        Ok(FileEntry {
            name: String::new(),
            path: "/".into(),
            entry_type: EntryType::Directory,
            size: 0,
            location: 0,
            children: None,
            resource_fork_size: None,
            type_code: None,
            creator_code: None,
            finder_flags: None,
            symlink_target: None,
            timestamps: None,
            posix: None,
        })
    }

    fn list_directory(&mut self, entry: &FileEntry) -> Result<Vec<FileEntry>, FilesystemError> {
        let fst = self
            .meta
            .fst()
            .map_err(|e| FilesystemError::Parse(e.into()))?;
        let dir_idx = entry.location as usize;
        let dir = fst
            .nodes
            .get(dir_idx)
            .copied()
            .ok_or_else(|| FilesystemError::NotFound(entry.path.clone()))?;
        if !dir.is_dir() {
            return Err(FilesystemError::NotADirectory(entry.path.clone()));
        }

        // A directory node's `length()` is the index one past its last
        // descendant. Direct children are enumerated by skipping over each
        // subdirectory's whole subtree.
        let end = (dir.length() as usize).min(fst.nodes.len());
        let base = entry.path.trim_end_matches('/');
        let mut out = Vec::new();
        let mut i = dir_idx + 1;
        while i < end {
            let child = fst.nodes[i];
            let name = fst
                .get_name(child)
                .map_err(FilesystemError::Parse)?
                .into_owned();
            let path = format!("{base}/{name}");
            if child.is_dir() {
                out.push(dir_entry(name, path, i));
                // Skip the subtree; `length()` is its child-end index.
                let next = (child.length() as usize).max(i + 1);
                i = next;
            } else {
                out.push(file_entry(name, path, child.length(), i));
                i += 1;
            }
        }
        Ok(out)
    }

    fn read_file(&mut self, entry: &FileEntry) -> Result<Vec<u8>, FilesystemError> {
        let node = self.node_at(entry.location as usize)?;
        if !node.is_file() {
            return Err(FilesystemError::NotFound(format!(
                "{} is not a file",
                entry.path
            )));
        }
        let mut stream = self
            .partition
            .open_file(node)
            .map_err(FilesystemError::Io)?;
        let mut buf = Vec::with_capacity(node.length() as usize);
        stream.read_to_end(&mut buf).map_err(FilesystemError::Io)?;
        Ok(buf)
    }

    fn read_file_range(
        &mut self,
        entry: &FileEntry,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FilesystemError> {
        let node = self.node_at(entry.location as usize)?;
        if !node.is_file() {
            return Err(FilesystemError::NotFound(format!(
                "{} is not a file",
                entry.path
            )));
        }
        let mut stream = self
            .partition
            .open_file(node)
            .map_err(FilesystemError::Io)?;
        stream
            .seek(SeekFrom::Start(offset))
            .map_err(FilesystemError::Io)?;
        let mut buf = vec![0u8; length];
        let n = read_up_to(&mut stream, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn read_resource_fork(
        &mut self,
        _entry: &FileEntry,
    ) -> Result<Option<Vec<u8>>, FilesystemError> {
        Ok(None)
    }

    fn read_resource_fork_range(
        &mut self,
        _entry: &FileEntry,
        _offset: u64,
        _length: usize,
    ) -> Result<Option<Vec<u8>>, FilesystemError> {
        Ok(None)
    }

    fn volume_name(&self) -> Option<&str> {
        if self.volume_name.is_empty() {
            None
        } else {
            Some(&self.volume_name)
        }
    }
}

// ── Detection helper ──────────────────────────────────────────────────────────

/// Probe `path` for a GameCube or Wii disc image via `nod`.
///
/// Returns the matching [`FilesystemType`] and a populated [`GameDiscInfo`], or
/// `None` if the file is not a recognizable Nintendo disc (including plain data
/// ISOs, for which `nod` returns an error we swallow).
pub fn probe_nintendo(path: &Path) -> Option<(FilesystemType, GameDiscInfo)> {
    let disc = Disc::new(path).ok()?;
    let header = disc.header();
    let (fs, console) = if header.is_wii() {
        (FilesystemType::Wii, Console::Wii)
    } else if header.is_gamecube() {
        (FilesystemType::GameCube, Console::GameCube)
    } else {
        return None;
    };

    let serial = ascii_z(&header.game_id);
    let title = ascii_z(&header.game_title);
    let region = header.game_id.get(3).and_then(|&c| nintendo_region(c));
    let maker = header
        .game_id
        .get(4..6)
        .map(ascii_z)
        .filter(|m| !m.is_empty());

    Some((
        fs,
        GameDiscInfo {
            console,
            serial: (!serial.is_empty()).then_some(serial),
            title: (!title.is_empty()).then_some(title),
            region,
            maker,
            version: Some(format!("rev {}", header.disc_version)),
        },
    ))
}

/// Map a Nintendo game-ID region character (4th byte of the game code) to a
/// coarse [`Region`].
fn nintendo_region(c: u8) -> Option<Region> {
    match c {
        b'E' | b'N' => Some(Region::NtscU),
        b'J' => Some(Region::NtscJ),
        b'P' | b'D' | b'F' | b'I' | b'S' | b'H' | b'X' | b'Y' | b'Z' | b'U' => Some(Region::Pal),
        b'K' | b'Q' | b'T' => Some(Region::Korea),
        b'W' => Some(Region::Asia),
        _ => None,
    }
}

/// True if `path` is an **NKit**-processed GC/Wii image.
///
/// NKit stores a `NKIT` marker at byte `0x200`, right after the preserved
/// 0x200-byte disc header. Such images cannot be browsed without reconstruction,
/// which `nod` does not perform.
pub fn is_nkit_image(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    if f.seek(SeekFrom::Start(0x200)).is_err() {
        return false;
    }
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && &magic == b"NKIT"
}

// ── Small helpers ─────────────────────────────────────────────────────────────

fn dir_entry(name: String, path: String, idx: usize) -> FileEntry {
    FileEntry {
        name,
        path,
        entry_type: EntryType::Directory,
        size: 0,
        location: idx as u64,
        children: None,
        resource_fork_size: None,
        type_code: None,
        creator_code: None,
        finder_flags: None,
        symlink_target: None,
        timestamps: None,
        posix: None,
    }
}

fn file_entry(name: String, path: String, size: u64, idx: usize) -> FileEntry {
    FileEntry {
        name,
        path,
        entry_type: EntryType::File,
        size,
        location: idx as u64,
        children: None,
        resource_fork_size: None,
        type_code: None,
        creator_code: None,
        finder_flags: None,
        symlink_target: None,
        timestamps: None,
        posix: None,
    }
}

/// Decode a fixed-size, null-padded ASCII byte field to a trimmed `String`.
fn ascii_z(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// Read as many bytes as possible into `buf`, stopping at EOF; returns the count.
fn read_up_to(stream: &mut impl Read, buf: &mut [u8]) -> Result<usize, FilesystemError> {
    let mut total = 0;
    while total < buf.len() {
        match stream.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FilesystemError::Io(e)),
        }
    }
    Ok(total)
}

/// Convert a `nod` error into a [`FilesystemError`].
fn nod_err(e: nod::Error) -> FilesystemError {
    FilesystemError::InvalidData(format!("nod: {e}"))
}
