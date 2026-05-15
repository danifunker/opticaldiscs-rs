# EFS (SGI Extent File System) Implementation Plan

Read-only support for SGI Volume Header + EFS filesystems on optical disc
images (ISO / BIN+CUE / CHD), parallel to the existing HFS/HFS+ path.

This plan is trackable: tick the `- [ ]` boxes as work lands. Each phase is
self-contained and ends with passing tests.

## Scope decisions

- **XFS is out of scope** for this work. All five sample IRIX CDs in
  `~/irixCDs/` (IRIX 4.0.4, IRIX 5.3, IRIX 5.3 "XFS", IRIX 6.4.1, IRIX
  6.5.29) carry an SGI Volume Header but the actual filesystem on every
  partition is **EFS** (magic `0x00072959` at sb+28). No XFS sample disc
  is available, so the XFS port from rusty-backup is deferred indefinitely.
- **Target version bump:** `0.2.0` → **`0.3.0`** (additive feature).
- **Synthetic EFS fixture** is committed to the repo for CI; large IRIX
  ISOs stay out of the repo and are exercised by env-var-gated tests.

## Sample-disc reconnaissance (already done)

All sample ISOs have SGI Volume Header magic `0x0BE5A941`. Partition table
at offset `0x138` (sixteen 12-byte entries: `blocks`, `first`, `type`, all
big-endian 32-bit). The IRIX install CDs label the data partition with
**type byte 5 (SYSV)** rather than 7 (EFS); we must detect by reading the
EFS superblock magic, not by trusting the partition-type byte.

| File | Data partition | First (512-byte blocks) | FS at first×512 + 512 |
|---|---|---|---|
| IRIX 5.3 XFS.iso | part[7] type=SYSV | 60768 | EFS `0x00072959` |
| IRIX 6.4.1 (Origin, Octane).iso | part[7] type=SYSV | 62272 | EFS `0x00072959` |
| IRIX-53.iso | part[7] type=SYSV | 56864 | EFS `0x00072959` |
| sgi-mips-irix4.0.4.bin | part[7] type=SYSV | 41408 | EFS `0x00072959` |
| Irix 6.5.29 Installation & Overlays (1-3).iso | part[7] type=SYSV | 49248 | EFS `0x00072959` |

EFS superblock layout (big-endian, alignment-padded, magic at sb+28):

| Offset | Type | Field |
|--------|------|-------|
| 0 | be32 | fs_size |
| 4 | be32 | fs_firstcg |
| 8 | be32 | fs_cgfsize |
| 12 | be16 | fs_cgisize |
| 14 | be16 | fs_sectors |
| 16 | be16 | fs_heads |
| 18 | be16 | fs_ncg |
| 20 | be16 | fs_dirty |
| 22 | (2 pad) | — |
| 24 | be32 | fs_time |
| 28 | be32 | fs_magic |
| 32 | char[6] | fs_fname |
| 38 | char[6] | fs_fpack |
| 44 | be32 | fs_bmsize |
| 48 | be32 | fs_tfree |
| 52 | be32 | fs_tinode |
| 56 | be32 | fs_bmblock |
| 60 | be32 | fs_replsb |
| 64 | be32 | fs_lastialloc |
| 88 | be32 | fs_checksum |

## Reference sources

- `~/repos/rusty-backup/src/partition/sgi.rs` — SGI volume header parser.
- `~/repos/rusty-backup/src/fs/efs.rs` — EFS reader (1006 lines, complete).
- `~/repos/rusty-backup/docs/SGI_Filesystems.md` — on-disk format notes.
- Linux v5.15 `fs/efs/` — last kernel with EFS read-only support.

---

## Phase A — Foundations

- [x] Bump `Cargo.toml` `version = "0.2.0"` → `"0.3.0"`.
- [x] Add `FilesystemType::Efs` to `src/formats.rs`; update `display_name()`
      (`"EFS"`) and `is_browsable()` (include `Efs`).
- [x] Create `src/sgi.rs`:
  - [x] `SgiVolumeHeader` struct with magic, volume directory entries, and
        16 partition entries.
  - [x] `SgiPartitionEntry { blocks: u32, first: u32, ptype: u32 }`.
  - [x] `SgiPartitionType` enum (VOLHDR/SYSV/EFS/XFS/VOLUME/... — keep all
        IRIX values for display; only EFS/SYSV are acted on).
  - [x] `SgiVolumeHeader::read_from(reader: &mut dyn SectorReader) -> Result<Self>`
        using `read_bytes(0, 512)`. Validate magic `0x0BE5A941`; checksum
        verified but not enforced (log warning on mismatch).
- [x] Re-export `SgiVolumeHeader` from `lib.rs` (mirror the HFS pattern).
- [x] Unit tests in `sgi.rs`: parse a hand-built header buffer; reject
      wrong magic; correctly decode all 16 partition slots.

## Phase B — Detection

- [ ] Extend `detect.rs::probe_filesystem` with a new branch placed
      **after** the HFS check and **before** the APM check:
  - [ ] If `read_bytes(0, 4)` yields `0x0BE5A941`, parse the SGI volume
        header.
  - [ ] Walk partition entries; for each entry whose type is EFS (7) **or
        SYSV (5)** (covers the bundled IRIX install CDs), probe at
        `first × 512 + 512`: read 32 bytes, check `u32` at offset 28
        against EFS magic `0x00072959` or `0x0007295A`. First hit wins.
  - [ ] Return `(FilesystemType::Efs, None)` and stash the partition byte
        offset for downstream consumers.
- [ ] Extend `DiscImageInfo` (`detect.rs`):
  - [ ] `pub sgi_header: Option<SgiVolumeHeader>`.
  - [ ] `pub efs_partition_offset: Option<u64>` (byte offset within the
        sector reader).
- [ ] Populate the new fields in all three probe paths: `probe_iso`,
      `probe_bincue`, `probe_chd`. Factor the SGI/EFS detail-probe into a
      `probe_sgi_detail` helper alongside the existing `probe_hfs_detail`.
- [ ] Unit test: synthetic image with SGI header + EFS superblock placed
      at the partition offset is detected as `FilesystemType::Efs`.

## Phase C — EFS filesystem core (`src/efs.rs`)

Port from `rusty-backup/src/fs/efs.rs`. Adapt every `Read+Seek` call to
`SectorReader::read_bytes`. Drop the bounce-buffer alignment helper —
the existing sector readers already enforce 2048-byte alignment.

- [ ] `EfsSuperblock` with explicit byte-offset reads (magic at sb+28).
- [ ] `EfsExtent` (16-byte packed record: `magic:8, bn:24, off:8, len:8`).
- [ ] `EfsInode` (128 bytes, 12 inline extents, mode + size + nlinks).
- [ ] `EfsFilesystem::open(reader, partition_offset)` → parses superblock.
- [ ] `inode_byte_offset(inum)` using `fs_firstcg` / `fs_cgisize` /
      `fs_cgfsize` (port the cylinder-group math verbatim).
- [ ] `read_inode(inum) -> EfsInode`.
- [ ] `read_dir_block(buf, parent)` parsing the 512-byte slotted block
      format (`firstused:16, slots:8, magic:16`, slot offsets, then
      variable-length `(inumber:32, namelen:8, name[])` entries).
- [ ] `read_inode_data` / `stream_inode_data` walking the 12 inline
      extents (no indirect blocks in EFS — max ~192 MiB per file).
- [ ] Unit tests: parse superblock from the synthetic fixture; reject
      bad magic; inode-number → byte-offset translation matches a hand
      computation; directory block parse round-trip.

## Phase D — `Filesystem` trait impl (`src/browse/efs.rs`)

- [ ] `EfsFilesystem`-backed browser implementing `Filesystem`:
  - [ ] `root()` — inode 2 as a `FileEntry`.
  - [ ] `list_directory(entry)` — walk inode extents, parse dir blocks.
  - [ ] `read_file(entry)` — read all extents into a `Vec<u8>`.
  - [ ] `read_file_range(entry, offset, length)` — read a window.
  - [ ] `read_resource_fork{,_range}` → `Ok(None)`.
  - [ ] `volume_name()` from `fs_fname` + `fs_fpack` (Latin-1, trimmed).
- [ ] Symlinks: mode bit `0o120000`; surface as `EntryType::Symlink`
      with `target = read_file(entry).to_string_lossy()`.
- [ ] `FileEntry.location` carries the EFS inode number (parallel to LBA
      for ISO 9660 and CNID for HFS/HFS+).
- [ ] Register in `browse/mod.rs::open_filesystem` so the existing entry
      point dispatches on `FilesystemType::Efs`.

## Phase E — Public surface

- [ ] Re-export `EfsSuperblock`, `EfsFilesystem`, `SgiVolumeHeader` from
      `lib.rs`.
- [ ] Update the README filesystem-support table to list EFS and SGI
      Volume Header (read-only browse + extract).

## Phase F — Tests & fixtures

- [ ] Commit a small synthetic EFS fixture under `tests/fixtures/`:
  - [ ] `efs_synth.img` — hand-built, ≤ 512 KiB. SGI volume header +
        EFS partition with: 2-3 files at the root, one subdirectory
        with one nested file, one symlink. Built by a `#[cfg(test)]`
        builder helper so the buffer can be regenerated from code.
- [ ] Integration test `tests/efs_synth.rs`:
  - [ ] `DiscImageInfo::open` reports `format=Iso, filesystem=Efs`.
  - [ ] Volume label and known file bytes match expectations.
  - [ ] Browse descends into the subdirectory; symlink target resolves.
- [ ] Env-var-gated integration test `tests/efs_irix_samples.rs`
      (skipped unless `OPTICALDISCS_IRIX_CDS=~/irixCDs/` is set):
  - [ ] Each of the five IRIX ISOs opens, reports `Efs`, lists `/`
        without error, and exposes a non-empty volume label.

## Phase G — Docs

- [ ] Update README with EFS in the supported-filesystem table.
- [ ] Tick the checkboxes in this file as phases complete.

---

## Acceptance

When every box above is ticked: `cargo test` passes (including the
synthetic-fixture integration test), `cargo clippy -- -D warnings` is
clean, and opening any of the five IRIX CDs via `DiscImageInfo::open`
yields a browsable EFS filesystem.
