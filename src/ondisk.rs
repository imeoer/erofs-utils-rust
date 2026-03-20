// EROFS on-disk format constants and serialization helpers.
//
// Reference: include/erofs_fs.h

// Superblock magic and position
pub const EROFS_SUPER_MAGIC_V1: u32 = 0xE0F5_E1E2;
pub const EROFS_SUPER_OFFSET: u64 = 1024;
pub const EROFS_SB_BASE_SIZE: usize = 128; // base superblock without extensions

// Block / slot sizing
pub const EROFS_BLOCK_SIZE: u32 = 4096;
pub const EROFS_BLKSZBITS: u8 = 12;
pub const EROFS_ISLOTBITS: u32 = 5;
pub const EROFS_SLOTSIZE: u32 = 1 << EROFS_ISLOTBITS; // 32

// Feature compat flags
pub const EROFS_FEATURE_COMPAT_SB_CHKSUM: u32 = 0x0000_0001;
pub const EROFS_FEATURE_COMPAT_MTIME: u32 = 0x0000_0002;

// Feature incompat flags
pub const EROFS_FEATURE_INCOMPAT_CHUNKED_FILE: u32 = 0x0000_0004;
pub const EROFS_FEATURE_INCOMPAT_DEVICE_TABLE: u32 = 0x0000_0008;

// Inode data layout values (stored in i_format bits 1-3)
pub const EROFS_INODE_FLAT_PLAIN: u16 = 0;
pub const EROFS_INODE_FLAT_INLINE: u16 = 2;
pub const EROFS_INODE_CHUNK_BASED: u16 = 4;

// Inode format bit positions
pub const EROFS_INODE_LAYOUT_COMPACT: u16 = 0;
pub const EROFS_INODE_LAYOUT_EXTENDED: u16 = 1;
pub const EROFS_I_VERSION_BIT: u16 = 0;
pub const EROFS_I_DATALAYOUT_BIT: u16 = 1;
pub const EROFS_I_NLINK_1_BIT: u16 = 4; // non-directory compact inodes

// Chunk format flags (stored in erofs_inode_chunk_info.format)
pub const EROFS_CHUNK_FORMAT_INDEXES: u16 = 0x0020;

// On-disk struct sizes (verified against BUILD_BUG_ON in erofs_fs.h)
pub const EROFS_INODE_COMPACT_SIZE: usize = 32;
pub const EROFS_INODE_EXTENDED_SIZE: usize = 64;
pub const EROFS_CHUNK_INDEX_SIZE: usize = 8;
pub const EROFS_DIRENT_SIZE: usize = 12;
pub const EROFS_DEVICESLOT_SIZE: usize = 128;

// File type constants for directory entries
pub const EROFS_FT_REG_FILE: u8 = 1;
pub const EROFS_FT_DIR: u8 = 2;
pub const EROFS_FT_CHRDEV: u8 = 3;
pub const EROFS_FT_BLKDEV: u8 = 4;
pub const EROFS_FT_FIFO: u8 = 5;
pub const EROFS_FT_SOCK: u8 = 6;
pub const EROFS_FT_SYMLINK: u8 = 7;

pub const EROFS_NULL_ADDR: u64 = u64::MAX;

/// Compute i_format for a compact inode.
pub fn compact_i_format(datalayout: u16, nlink_1: bool) -> u16 {
    let mut fmt = (EROFS_INODE_LAYOUT_COMPACT << EROFS_I_VERSION_BIT)
        | (datalayout << EROFS_I_DATALAYOUT_BIT);
    if nlink_1 {
        fmt |= 1 << EROFS_I_NLINK_1_BIT;
    }
    fmt
}

/// Compute i_format for an extended inode.
pub fn extended_i_format(datalayout: u16) -> u16 {
    (EROFS_INODE_LAYOUT_EXTENDED << EROFS_I_VERSION_BIT) | (datalayout << EROFS_I_DATALAYOUT_BIT)
}

/// Serialize a 32-byte compact inode to bytes.
///
/// Layout (offsets within the 32 bytes):
///   0..2   i_format     (le16)
///   2..4   i_xattr_icount (le16) = 0
///   4..6   i_mode       (le16)
///   6..8   i_nb         (le16) — startblk_hi / nlink / blocks_hi
///   8..12  i_size       (le32)
///  12..16  i_mtime      (le32) — delta from epoch for compact inodes
///  16..20  i_u          (le32) — startblk_lo / rdev / chunk_info
///  20..24  i_ino        (le32) — stat compat inode number
///  24..26  i_uid        (le16)
///  26..28  i_gid        (le16)
///  28..32  i_reserved   (le32) = 0
#[allow(clippy::too_many_arguments)]
pub fn serialize_inode_compact(
    i_format: u16,
    i_mode: u16,
    i_nb: u16,
    i_size: u32,
    i_mtime: u32,
    i_u: u32,
    i_ino: u32,
    i_uid: u16,
    i_gid: u16,
) -> [u8; EROFS_INODE_COMPACT_SIZE] {
    let mut buf = [0u8; EROFS_INODE_COMPACT_SIZE];
    buf[0..2].copy_from_slice(&i_format.to_le_bytes());
    // i_xattr_icount = 0 (bytes 2..4 already zero)
    buf[4..6].copy_from_slice(&i_mode.to_le_bytes());
    buf[6..8].copy_from_slice(&i_nb.to_le_bytes());
    buf[8..12].copy_from_slice(&i_size.to_le_bytes());
    buf[12..16].copy_from_slice(&i_mtime.to_le_bytes());
    buf[16..20].copy_from_slice(&i_u.to_le_bytes());
    buf[20..24].copy_from_slice(&i_ino.to_le_bytes());
    buf[24..26].copy_from_slice(&i_uid.to_le_bytes());
    buf[26..28].copy_from_slice(&i_gid.to_le_bytes());
    // i_reserved = 0 (bytes 28..32 already zero)
    buf
}

/// Serialize a 64-byte extended inode to bytes.
///
/// Layout:
///   0..2   i_format     (le16)
///   2..4   i_xattr_icount (le16) = 0
///   4..6   i_mode       (le16)
///   6..8   i_nb         (le16) — startblk_hi / blocks_hi
///   8..16  i_size       (le64)
///  16..20  i_u          (le32) — startblk_lo / rdev / chunk_info
///  20..24  i_ino        (le32)
///  24..28  i_uid        (le32)
///  28..32  i_gid        (le32)
///  32..40  i_mtime      (le64) — absolute timestamp
///  40..44  i_mtime_nsec (le32)
///  44..48  i_nlink      (le32)
///  48..64  i_reserved2  (16 bytes of zeros)
#[allow(clippy::too_many_arguments)]
pub fn serialize_inode_extended(
    i_format: u16,
    i_mode: u16,
    i_nb: u16,
    i_size: u64,
    i_u: u32,
    i_ino: u32,
    i_uid: u32,
    i_gid: u32,
    i_mtime: u64,
    i_mtime_nsec: u32,
    i_nlink: u32,
) -> [u8; EROFS_INODE_EXTENDED_SIZE] {
    let mut buf = [0u8; EROFS_INODE_EXTENDED_SIZE];
    buf[0..2].copy_from_slice(&i_format.to_le_bytes());
    // i_xattr_icount = 0
    buf[4..6].copy_from_slice(&i_mode.to_le_bytes());
    buf[6..8].copy_from_slice(&i_nb.to_le_bytes());
    buf[8..16].copy_from_slice(&i_size.to_le_bytes());
    buf[16..20].copy_from_slice(&i_u.to_le_bytes());
    buf[20..24].copy_from_slice(&i_ino.to_le_bytes());
    buf[24..28].copy_from_slice(&i_uid.to_le_bytes());
    buf[28..32].copy_from_slice(&i_gid.to_le_bytes());
    buf[32..40].copy_from_slice(&i_mtime.to_le_bytes());
    buf[40..44].copy_from_slice(&i_mtime_nsec.to_le_bytes());
    buf[44..48].copy_from_slice(&i_nlink.to_le_bytes());
    // i_reserved2 = 0 (bytes 48..64 already zero)
    buf
}

/// Serialize a chunk index (8 bytes).
///
/// Layout:
///   0..2  startblk_hi  (le16)
///   2..4  device_id    (le16)
///   4..8  startblk_lo  (le32)
pub fn serialize_chunk_index(blkaddr: u64, device_id: u16) -> [u8; EROFS_CHUNK_INDEX_SIZE] {
    let mut buf = [0u8; EROFS_CHUNK_INDEX_SIZE];
    if blkaddr == EROFS_NULL_ADDR {
        // Hole: all 0xFF
        buf.fill(0xFF);
    } else {
        let lo = blkaddr as u32;
        let hi = (blkaddr >> 32) as u16;
        buf[0..2].copy_from_slice(&hi.to_le_bytes());
        buf[2..4].copy_from_slice(&device_id.to_le_bytes());
        buf[4..8].copy_from_slice(&lo.to_le_bytes());
    }
    buf
}

/// Serialize a directory entry (12 bytes).
///
/// Layout:
///   0..8   nid        (le64)
///   8..10  nameoff    (le16)
///  10..11  file_type  (u8)
///  11..12  reserved   (u8) = 0
pub fn serialize_dirent(nid: u64, nameoff: u16, file_type: u8) -> [u8; EROFS_DIRENT_SIZE] {
    let mut buf = [0u8; EROFS_DIRENT_SIZE];
    buf[0..8].copy_from_slice(&nid.to_le_bytes());
    buf[8..10].copy_from_slice(&nameoff.to_le_bytes());
    buf[10] = file_type;
    buf
}

/// Serialize a device slot (128 bytes).
///
/// Layout:
///   0..64   tag         (64 bytes, zeros for us)
///  64..68   blocks_lo   (le32)
///  68..72   uniaddr_lo  (le32) = 0
///  72..76   blocks_hi   (le32) = 0
///  76..78   uniaddr_hi  (le16) = 0
///  78..128  reserved    (50 bytes) = 0
pub fn serialize_device_slot(blocks: u64) -> [u8; EROFS_DEVICESLOT_SIZE] {
    let mut buf = [0u8; EROFS_DEVICESLOT_SIZE];
    buf[64..68].copy_from_slice(&(blocks as u32).to_le_bytes());
    buf[72..76].copy_from_slice(&((blocks >> 32) as u32).to_le_bytes());
    buf
}

/// Serialize the superblock (128 bytes, sb_extslots=0).
///
/// Reference: struct erofs_super_block in erofs_fs.h.
#[allow(clippy::too_many_arguments)]
pub fn serialize_superblock(
    feature_compat: u32,
    feature_incompat: u32,
    root_nid: u16,
    inos: u64,
    epoch: u64,
    blocks: u64,
    meta_blkaddr: u32,
    extra_devices: u16,
    devt_slotoff: u16,
    uuid: &[u8; 16],
) -> [u8; EROFS_SB_BASE_SIZE] {
    let mut buf = [0u8; EROFS_SB_BASE_SIZE];

    // +0: magic
    buf[0..4].copy_from_slice(&EROFS_SUPER_MAGIC_V1.to_le_bytes());
    // +4: checksum = 0 (filled later)
    // +8: feature_compat (without SB_CHKSUM, added after CRC)
    buf[8..12].copy_from_slice(&(feature_compat & !EROFS_FEATURE_COMPAT_SB_CHKSUM).to_le_bytes());
    // +12: blkszbits
    buf[12] = EROFS_BLKSZBITS;
    // +13: sb_extslots = 0
    // +14: rb.rootnid_2b
    buf[14..16].copy_from_slice(&root_nid.to_le_bytes());
    // +16: inos
    buf[16..24].copy_from_slice(&inos.to_le_bytes());
    // +24: epoch
    buf[24..32].copy_from_slice(&epoch.to_le_bytes());
    // +32: fixed_nsec = 0
    // +36: blocks_lo
    buf[36..40].copy_from_slice(&(blocks as u32).to_le_bytes());
    // +40: meta_blkaddr
    buf[40..44].copy_from_slice(&meta_blkaddr.to_le_bytes());
    // +44: xattr_blkaddr = 0
    // +48: uuid
    buf[48..64].copy_from_slice(uuid);
    // +64: volume_name (zeros)
    // +80: feature_incompat
    buf[80..84].copy_from_slice(&feature_incompat.to_le_bytes());
    // +84: available_compr_algs / lz4_max_distance = 0
    // +86: extra_devices
    buf[86..88].copy_from_slice(&extra_devices.to_le_bytes());
    // +88: devt_slotoff
    buf[88..90].copy_from_slice(&devt_slotoff.to_le_bytes());
    // +90: dirblkbits = 0
    // +91..108: various reserved/unused fields = 0
    // +108: build_time = 0 (seconds added to epoch)
    // +112: rootnid_8b = 0 (not used without 48BIT)
    buf
}

/// Round `val` up to the next multiple of `align`. `align` must be a power of two.
pub fn round_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}
