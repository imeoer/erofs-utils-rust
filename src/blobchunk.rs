use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use blake3;

use crate::ondisk::EROFS_BLOCK_SIZE;

/// Represents a chunk stored in the blob device.
#[derive(Clone)]
struct BlobChunk {
    /// Block address within the blob device.
    blkaddr: u64,
}

/// Information about a single chunk index to be stored in an inode.
#[derive(Clone)]
pub struct ChunkIndex {
    /// Block address in the blob device.
    pub blkaddr: u64,
    /// Device ID (1 for the blobdev).
    pub device_id: u16,
}

/// Manages writing chunk data to a separate blob device with SHA256 dedup.
pub struct BlobWriter {
    file: File,
    chunksize: u32,
    /// Current write position in blocks.
    next_blkaddr: u64,
    /// SHA256 → BlobChunk dedup index.
    dedup: HashMap<[u8; 32], BlobChunk>,
    /// Total bytes saved by dedup.
    pub saved_by_dedup: u64,
}

impl BlobWriter {
    pub fn new(path: &Path, chunksize: u32) -> Result<Self> {
        let file = File::create(path)
            .with_context(|| format!("failed to create blob device: {}", path.display()))?;
        Ok(Self {
            file,
            chunksize,
            next_blkaddr: 0,
            dedup: HashMap::new(),
            saved_by_dedup: 0,
        })
    }

    /// Total number of blocks written to the blob device.
    pub fn total_blocks(&self) -> u64 {
        self.next_blkaddr
    }

    /// Process a regular file: read it in chunk-sized pieces, dedup via SHA256,
    /// write unique chunks to the blob device.
    /// Returns a list of ChunkIndex entries for the inode.
    pub fn write_file_chunks(&mut self, path: &Path, file_size: u64) -> Result<Vec<ChunkIndex>> {
        if file_size == 0 {
            return Ok(Vec::new());
        }

        let mut f =
            File::open(path).with_context(|| format!("failed to open file: {}", path.display()))?;

        let cs = self.chunksize as u64;
        let nchunks = file_size.div_ceil(cs);
        let mut indexes = Vec::with_capacity(nchunks as usize);
        let mut chunk_buf = vec![0u8; self.chunksize as usize];

        for i in 0..nchunks {
            let remaining = file_size - i * cs;
            let to_read = remaining.min(cs) as usize;

            // Read actual data
            f.read_exact(&mut chunk_buf[..to_read])
                .with_context(|| format!("failed to read file: {}", path.display()))?;

            // Only hash the actual data bytes (not padded to chunksize).
            // The C version also hashes only the real data length per chunk.
            let hash: [u8; 32] = *blake3::hash(&chunk_buf[..to_read]).as_bytes();

            // Actual blocks needed = ceil(to_read / BLOCK_SIZE)
            let write_len = to_read.div_ceil(EROFS_BLOCK_SIZE as usize) * EROFS_BLOCK_SIZE as usize;
            let nblocks = (write_len / EROFS_BLOCK_SIZE as usize) as u64;

            let blkaddr = if let Some(existing) = self.dedup.get(&hash) {
                self.saved_by_dedup += write_len as u64;
                existing.blkaddr
            } else {
                let addr = self.next_blkaddr;
                // Write actual data + zero-pad to block boundary
                self.file
                    .write_all(&chunk_buf[..to_read])
                    .context("failed to write to blob device")?;
                if write_len > to_read {
                    let padding = vec![0u8; write_len - to_read];
                    self.file
                        .write_all(&padding)
                        .context("failed to write padding to blob device")?;
                }
                self.next_blkaddr += nblocks;
                self.dedup.insert(hash, BlobChunk { blkaddr: addr });
                addr
            };

            indexes.push(ChunkIndex {
                blkaddr,
                device_id: 1, // blobdev is always device 1
            });
        }
        Ok(indexes)
    }
}
