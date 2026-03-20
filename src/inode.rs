use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::blobchunk::{BlobWriter, ChunkIndex};
use crate::ondisk::*;

/// In-memory inode representation.
pub struct InodeInfo {
    // Stat fields
    pub mode: u16,     // full mode including file type bits
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime: u64,       // seconds since epoch
    pub mtime_nsec: u32,
    pub nlink: u32,
    pub ino: u32,         // assigned inode number (for stat compat)

    // Layout info (set during allocation)
    pub nid: u64,         // assigned EROFS NID
    pub meta_offset: usize, // offset in metadata buffer

    /// True if this inode needs extended (64-byte) format.
    pub is_extended: bool,

    /// File-type-specific data.
    pub data: InodeData,
}

pub enum InodeData {
    /// Regular file: chunk indexes for chunk-based layout.
    RegularFile {
        chunk_indexes: Vec<ChunkIndex>,
    },
    /// Directory: sorted children.
    Directory {
        children: Vec<DirEntry>,
        /// Absolute start block of directory data (set during layout).
        startblk: u64,
        /// Serialized directory data byte length.
        dir_data_size: usize,
        /// Parent NID (set during layout).
        parent_nid: u64,
    },
    /// Symbolic link: target path.
    Symlink {
        target: Vec<u8>,
    },
    /// Character/block device.
    SpecialDev {
        rdev: u32,
    },
    /// FIFO or socket (no data).
    SpecialNoData,
}

/// A directory entry referencing a child inode.
pub struct DirEntry {
    pub name: String,
    pub file_type: u8,
    pub inode_idx: usize, // index into the flat inode list
}

/// Build the in-memory inode tree from a source directory.
///
/// Returns a flat list of InodeInfo in DFS pre-order (root at index 0).
/// Also writes chunk data to the blob device.
pub fn build_tree(
    source: &Path,
    blob_writer: &mut BlobWriter,
    chunksize: u32,
) -> Result<Vec<InodeInfo>> {
    let mut inodes: Vec<InodeInfo> = Vec::new();
    let mut ino_counter: u32 = 0;
    // Hardlink detection: (device, source_ino) → inode index
    let mut hardlink_map: HashMap<(u64, u64), usize> = HashMap::new();

    build_tree_recursive(
        source,
        blob_writer,
        chunksize,
        &mut inodes,
        &mut ino_counter,
        &mut hardlink_map,
    )?;

    Ok(inodes)
}

#[allow(clippy::only_used_in_recursion)]
fn build_tree_recursive(
    path: &Path,
    blob_writer: &mut BlobWriter,
    chunksize: u32,
    inodes: &mut Vec<InodeInfo>,
    ino_counter: &mut u32,
    hardlink_map: &mut HashMap<(u64, u64), usize>,
) -> Result<usize> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat: {}", path.display()))?;

    let ft = meta.file_type();
    let mode = meta.mode() as u16;
    let uid = meta.uid();
    let gid = meta.gid();
    let mtime = meta.mtime() as u64;
    let mtime_nsec = meta.mtime_nsec() as u32;
    let nlink = meta.nlink() as u32;

    // Check if we need extended inode
    let is_extended = meta.size() > u32::MAX as u64
        || uid > u16::MAX as u32
        || gid > u16::MAX as u32
        || nlink > 1;

    *ino_counter += 1;
    let ino = *ino_counter;

    if ft.is_dir() {
        // Create directory inode placeholder
        let inode_idx = inodes.len();
        inodes.push(InodeInfo {
            mode,
            uid,
            gid,
            size: 0, // set later after dir data serialization
            mtime,
            mtime_nsec,
            nlink,
            ino,
            nid: 0,
            meta_offset: 0,
            is_extended,
            data: InodeData::Directory {
                children: Vec::new(),
                startblk: 0,
                dir_data_size: 0,
                parent_nid: 0,
            },
        });

        // Read and sort directory entries
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(path)
            .with_context(|| format!("failed to read directory: {}", path.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("failed to iterate directory: {}", path.display()))?;
        entries.sort_by_key(|e| e.file_name());

        // Process children recursively
        let mut children = Vec::new();
        for entry in &entries {
            let child_path = entry.path();
            let child_meta = fs::symlink_metadata(&child_path)
                .with_context(|| format!("failed to stat: {}", child_path.display()))?;

            // Hardlink detection for non-directories
            if !child_meta.file_type().is_dir() && child_meta.nlink() > 1 {
                let key = (child_meta.dev(), child_meta.ino());
                if let Some(&existing_idx) = hardlink_map.get(&key) {
                    // This is a hardlink to an already-seen inode
                    let file_type = mode_to_file_type(child_meta.mode() as u16);
                    children.push(DirEntry {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        file_type,
                        inode_idx: existing_idx,
                    });
                    continue;
                }
            }

            let child_idx = build_tree_recursive(
                &child_path,
                blob_writer,
                chunksize,
                inodes,
                ino_counter,
                hardlink_map,
            )?;

            // Register hardlink mapping for non-directories with nlink > 1
            if !child_meta.file_type().is_dir() && child_meta.nlink() > 1 {
                let key = (child_meta.dev(), child_meta.ino());
                hardlink_map.insert(key, child_idx);
            }

            let file_type = mode_to_file_type(child_meta.mode() as u16);
            children.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                file_type,
                inode_idx: child_idx,
            });
        }

        // Update directory inode with children
        if let InodeData::Directory {
            children: ref mut dir_children,
            ..
        } = inodes[inode_idx].data
        {
            *dir_children = children;
        }

        Ok(inode_idx)
    } else if ft.is_file() {
        let file_size = meta.size();
        let chunk_indexes = blob_writer.write_file_chunks(path, file_size)?;

        let inode_idx = inodes.len();
        inodes.push(InodeInfo {
            mode,
            uid,
            gid,
            size: file_size,
            mtime,
            mtime_nsec,
            nlink,
            ino,
            nid: 0,
            meta_offset: 0,
            is_extended,
            data: InodeData::RegularFile { chunk_indexes },
        });
        Ok(inode_idx)
    } else if ft.is_symlink() {
        let target = fs::read_link(path)
            .with_context(|| format!("failed to read symlink: {}", path.display()))?;
        let target_bytes = target_to_bytes(&target);

        let inode_idx = inodes.len();
        inodes.push(InodeInfo {
            mode,
            uid,
            gid,
            size: target_bytes.len() as u64,
            mtime,
            mtime_nsec,
            nlink,
            ino,
            nid: 0,
            meta_offset: 0,
            is_extended,
            data: InodeData::Symlink {
                target: target_bytes,
            },
        });
        Ok(inode_idx)
    } else {
        // Special files: char, block, fifo, socket
        let rdev = meta.rdev() as u32;
        let is_dev = (mode & 0o170000) == 0o020000 || (mode & 0o170000) == 0o060000;

        let inode_idx = inodes.len();
        inodes.push(InodeInfo {
            mode,
            uid,
            gid,
            size: 0,
            mtime,
            mtime_nsec,
            nlink,
            ino,
            nid: 0,
            meta_offset: 0,
            is_extended,
            data: if is_dev {
                InodeData::SpecialDev { rdev }
            } else {
                InodeData::SpecialNoData
            },
        });
        Ok(inode_idx)
    }
}

fn target_to_bytes(target: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    target.as_os_str().as_bytes().to_vec()
}

/// Map S_IFMT to EROFS_FT_* constants.
fn mode_to_file_type(mode: u16) -> u8 {
    match mode & 0o170000 {
        0o100000 => EROFS_FT_REG_FILE,
        0o040000 => EROFS_FT_DIR,
        0o020000 => EROFS_FT_CHRDEV,
        0o060000 => EROFS_FT_BLKDEV,
        0o010000 => EROFS_FT_FIFO,
        0o140000 => EROFS_FT_SOCK,
        0o120000 => EROFS_FT_SYMLINK,
        _ => 0, // EROFS_FT_UNKNOWN
    }
}

/// Compute the metadata size for an inode.
pub fn inode_meta_size(inode: &InodeInfo, _chunkbits: u32, _blkszbits: u32) -> usize {
    let base = if inode.is_extended {
        EROFS_INODE_EXTENDED_SIZE
    } else {
        EROFS_INODE_COMPACT_SIZE
    };

    match &inode.data {
        InodeData::RegularFile { chunk_indexes } => {
            if chunk_indexes.is_empty() {
                base
            } else {
                // Chunk indexes must be 8-byte aligned from inode start
                let extent_offset = round_up(base, 8);
                extent_offset + chunk_indexes.len() * EROFS_CHUNK_INDEX_SIZE
            }
        }
        InodeData::Directory { .. } => {
            // Directory data is in separate blocks; inode is just the header
            base
        }
        InodeData::Symlink { target } => {
            // Symlink target is inline data right after the inode header
            base + target.len()
        }
        InodeData::SpecialDev { .. } | InodeData::SpecialNoData => base,
    }
}

/// Compute the chunk format value for chunk-based inodes.
pub fn chunk_format(chunkbits: u32, blkszbits: u32) -> u16 {
    EROFS_CHUNK_FORMAT_INDEXES | ((chunkbits - blkszbits) as u16)
}

/// Serialize an inode to bytes and write it at the given offset in a buffer.
pub fn serialize_inode(inode: &InodeInfo, epoch: u64, chunkbits: u32) -> Vec<u8> {
    let blkszbits = EROFS_BLKSZBITS as u32;
    let meta_size = inode_meta_size(inode, chunkbits, blkszbits);
    let mut buf = vec![0u8; meta_size];

    match &inode.data {
        InodeData::RegularFile { chunk_indexes } => {
            let datalayout = EROFS_INODE_CHUNK_BASED;
            let cf = chunk_format(chunkbits, blkszbits);
            // chunk_info format stored in i_u (lower 16 bits = format, upper 16 bits = reserved=0)
            let i_u = cf as u32;

            if inode.is_extended {
                let i_format = extended_i_format(datalayout);
                let hdr = serialize_inode_extended(
                    i_format,
                    inode.mode,
                    0, // i_nb: unused for chunk-based
                    inode.size,
                    i_u,
                    inode.ino,
                    inode.uid,
                    inode.gid,
                    inode.mtime,
                    inode.mtime_nsec,
                    inode.nlink,
                );
                buf[..EROFS_INODE_EXTENDED_SIZE].copy_from_slice(&hdr);
            } else {
                let i_format = compact_i_format(datalayout, true);
                let i_mtime = inode.mtime.wrapping_sub(epoch) as u32;
                let hdr = serialize_inode_compact(
                    i_format,
                    inode.mode,
                    0,
                    inode.size as u32,
                    i_mtime,
                    i_u,
                    inode.ino,
                    inode.uid as u16,
                    inode.gid as u16,
                );
                buf[..EROFS_INODE_COMPACT_SIZE].copy_from_slice(&hdr);
            }

            // Write chunk indexes after the inode header, 8-byte aligned
            let base = if inode.is_extended {
                EROFS_INODE_EXTENDED_SIZE
            } else {
                EROFS_INODE_COMPACT_SIZE
            };
            let extent_offset = round_up(base, 8);
            for (i, ci) in chunk_indexes.iter().enumerate() {
                let idx = serialize_chunk_index(ci.blkaddr, ci.device_id);
                let off = extent_offset + i * EROFS_CHUNK_INDEX_SIZE;
                buf[off..off + EROFS_CHUNK_INDEX_SIZE].copy_from_slice(&idx);
            }
        }
        InodeData::Directory { startblk, .. } => {
            let datalayout = EROFS_INODE_FLAT_PLAIN;
            let startblk_lo = *startblk as u32;
            let startblk_hi = (*startblk >> 32) as u16;

            if inode.is_extended {
                let i_format = extended_i_format(datalayout);
                let hdr = serialize_inode_extended(
                    i_format,
                    inode.mode,
                    startblk_hi,
                    inode.size,
                    startblk_lo,
                    inode.ino,
                    inode.uid,
                    inode.gid,
                    inode.mtime,
                    inode.mtime_nsec,
                    inode.nlink,
                );
                buf[..EROFS_INODE_EXTENDED_SIZE].copy_from_slice(&hdr);
            } else {
                let i_format = compact_i_format(datalayout, false);
                let i_mtime = inode.mtime.wrapping_sub(epoch) as u32;
                let hdr = serialize_inode_compact(
                    i_format,
                    inode.mode,
                    startblk_hi,
                    inode.size as u32,
                    i_mtime,
                    startblk_lo,
                    inode.ino,
                    inode.uid as u16,
                    inode.gid as u16,
                );
                buf[..EROFS_INODE_COMPACT_SIZE].copy_from_slice(&hdr);
            }
        }
        InodeData::Symlink { target } => {
            let datalayout = EROFS_INODE_FLAT_INLINE;

            if inode.is_extended {
                let i_format = extended_i_format(datalayout);
                let hdr = serialize_inode_extended(
                    i_format,
                    inode.mode,
                    0,
                    inode.size,
                    0,
                    inode.ino,
                    inode.uid,
                    inode.gid,
                    inode.mtime,
                    inode.mtime_nsec,
                    inode.nlink,
                );
                buf[..EROFS_INODE_EXTENDED_SIZE].copy_from_slice(&hdr);
                buf[EROFS_INODE_EXTENDED_SIZE..].copy_from_slice(target);
            } else {
                let i_format = compact_i_format(datalayout, true);
                let i_mtime = inode.mtime.wrapping_sub(epoch) as u32;
                let hdr = serialize_inode_compact(
                    i_format,
                    inode.mode,
                    0,
                    inode.size as u32,
                    i_mtime,
                    0,
                    inode.ino,
                    inode.uid as u16,
                    inode.gid as u16,
                );
                buf[..EROFS_INODE_COMPACT_SIZE].copy_from_slice(&hdr);
                buf[EROFS_INODE_COMPACT_SIZE..].copy_from_slice(target);
            }
        }
        InodeData::SpecialDev { rdev } => {
            let datalayout = EROFS_INODE_FLAT_PLAIN;

            if inode.is_extended {
                let i_format = extended_i_format(datalayout);
                let hdr = serialize_inode_extended(
                    i_format,
                    inode.mode,
                    0,
                    0,
                    *rdev,
                    inode.ino,
                    inode.uid,
                    inode.gid,
                    inode.mtime,
                    inode.mtime_nsec,
                    inode.nlink,
                );
                buf[..EROFS_INODE_EXTENDED_SIZE].copy_from_slice(&hdr);
            } else {
                let i_format = compact_i_format(datalayout, true);
                let i_mtime = inode.mtime.wrapping_sub(epoch) as u32;
                let hdr = serialize_inode_compact(
                    i_format,
                    inode.mode,
                    0,
                    0,
                    i_mtime,
                    *rdev,
                    inode.ino,
                    inode.uid as u16,
                    inode.gid as u16,
                );
                buf[..EROFS_INODE_COMPACT_SIZE].copy_from_slice(&hdr);
            }
        }
        InodeData::SpecialNoData => {
            let datalayout = EROFS_INODE_FLAT_PLAIN;

            if inode.is_extended {
                let i_format = extended_i_format(datalayout);
                let hdr = serialize_inode_extended(
                    i_format,
                    inode.mode,
                    0,
                    0,
                    0,
                    inode.ino,
                    inode.uid,
                    inode.gid,
                    inode.mtime,
                    inode.mtime_nsec,
                    inode.nlink,
                );
                buf[..EROFS_INODE_EXTENDED_SIZE].copy_from_slice(&hdr);
            } else {
                let i_format = compact_i_format(datalayout, true);
                let i_mtime = inode.mtime.wrapping_sub(epoch) as u32;
                let hdr = serialize_inode_compact(
                    i_format,
                    inode.mode,
                    0,
                    0,
                    i_mtime,
                    0,
                    inode.ino,
                    inode.uid as u16,
                    inode.gid as u16,
                );
                buf[..EROFS_INODE_COMPACT_SIZE].copy_from_slice(&hdr);
            }
        }
    }
    buf
}
