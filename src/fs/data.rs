use std::io;
use std::os::unix::io::AsRawFd;

use crate::metadata::*;

use super::ErofsReader;

impl ErofsReader {
    fn chunkbits(&self, inode: &ErofsInode<'_>) -> u32 {
        self.sb().blkszbits as u32 + (inode.chunk_format() as u32 & 0x1F)
    }

    fn chunk_indexes<'a>(
        &'a self,
        nid: u64,
        inode: &ErofsInode<'_>,
    ) -> io::Result<&'a [u8]> {
        if inode.data_layout() != EROFS_INODE_CHUNK_BASED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a chunk-based inode",
            ));
        }
        let chunkbits = self.chunkbits(inode);
        let chunksize = 1u64 << chunkbits;
        let nchunks = inode.size().div_ceil(chunksize) as usize;
        let inode_offset = self.nid_to_offset(nid);
        let header_size = inode.header_size() + inode.xattr_size();
        let ci_offset = inode_offset + header_size;
        let ci_total = nchunks * EROFS_CHUNK_INDEX_SIZE;
        self.mmap_slice(ci_offset, ci_total)
    }

    fn chunk_index_at<'a>(ci_data: &'a [u8], i: usize) -> &'a ErofsChunkIndex {
        let off = i * EROFS_CHUNK_INDEX_SIZE;
        cast_ref::<ErofsChunkIndex>(&ci_data[off..])
    }

    // ------------------------------------------------------------------
    // File data read — sync
    // ------------------------------------------------------------------

    pub fn read_file_data_sync(
        &self,
        nid: u64,
        inode: &ErofsInode<'_>,
        offset: u64,
        size: u32,
    ) -> io::Result<Vec<u8>> {
        if offset >= inode.size() {
            return Ok(Vec::new());
        }
        let actual_size = std::cmp::min(size as u64, inode.size() - offset) as usize;
        let layout = inode.data_layout();

        match layout {
            EROFS_INODE_FLAT_PLAIN | EROFS_INODE_FLAT_INLINE => {
                self.read_flat_data_vec(nid, inode, offset, actual_size)
            }
            EROFS_INODE_CHUNK_BASED => {
                self.read_chunk_data_sync(nid, inode, offset, actual_size)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported data layout: {}", layout),
            )),
        }
    }

    // ------------------------------------------------------------------
    // File data read — async
    // ------------------------------------------------------------------

    pub async fn read_file_data(
        &self,
        nid: u64,
        inode: &ErofsInode<'_>,
        offset: u64,
        size: u32,
    ) -> io::Result<Vec<u8>> {
        if offset >= inode.size() {
            return Ok(Vec::new());
        }
        let actual_size = std::cmp::min(size as u64, inode.size() - offset) as usize;
        let layout = inode.data_layout();

        match layout {
            EROFS_INODE_FLAT_PLAIN | EROFS_INODE_FLAT_INLINE => {
                self.read_flat_data_vec(nid, inode, offset, actual_size)
            }
            EROFS_INODE_CHUNK_BASED => {
                self.read_chunk_data_async(nid, inode, offset, actual_size)
                    .await
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported data layout: {}", layout),
            )),
        }
    }

    // ------------------------------------------------------------------
    // Chunk data — sync
    // ------------------------------------------------------------------

    fn read_chunk_data_sync(
        &self,
        nid: u64,
        inode: &ErofsInode<'_>,
        offset: u64,
        size: usize,
    ) -> io::Result<Vec<u8>> {
        let chunkbits = self.chunkbits(inode);
        let chunksize = 1u64 << chunkbits;
        let ci_data = self.chunk_indexes(nid, inode)?;
        let nchunks = inode.size().div_ceil(chunksize) as usize;

        let mut result = vec![0u8; size];
        let mut remaining = size;
        let mut file_pos = offset;
        let mut buf_pos = 0;

        while remaining > 0 {
            let chunk_idx = (file_pos / chunksize) as usize;
            let chunk_off = file_pos % chunksize;
            let to_read = std::cmp::min(remaining, (chunksize - chunk_off) as usize);

            if chunk_idx >= nchunks {
                break;
            }

            let ci = Self::chunk_index_at(ci_data, chunk_idx);
            let blkaddr = ci.blkaddr();
            if blkaddr == u64::MAX {
                // Hole — zeros
            } else if ci.device_id() > 0 {
                let blob_offset = blkaddr * EROFS_BLOCK_SIZE as u64 + chunk_off;
                self.blob_pread_sync(
                    &mut result[buf_pos..buf_pos + to_read],
                    blob_offset as i64,
                )?;
            } else {
                let data_offset =
                    (blkaddr * EROFS_BLOCK_SIZE as u64 + chunk_off) as usize;
                let slice = self.mmap_slice(data_offset, to_read)?;
                result[buf_pos..buf_pos + to_read].copy_from_slice(slice);
            }

            file_pos += to_read as u64;
            buf_pos += to_read;
            remaining -= to_read;
        }

        Ok(result)
    }

    // ------------------------------------------------------------------
    // Chunk data — async
    // ------------------------------------------------------------------

    async fn read_chunk_data_async(
        &self,
        nid: u64,
        inode: &ErofsInode<'_>,
        offset: u64,
        size: usize,
    ) -> io::Result<Vec<u8>> {
        let chunkbits = self.chunkbits(inode);
        let chunksize = 1u64 << chunkbits;
        let ci_data = self.chunk_indexes(nid, inode)?;
        let nchunks = inode.size().div_ceil(chunksize) as usize;

        let mut result = vec![0u8; size];
        let mut remaining = size;
        let mut file_pos = offset;
        let mut buf_pos = 0;

        while remaining > 0 {
            let chunk_idx = (file_pos / chunksize) as usize;
            let chunk_off = file_pos % chunksize;
            let to_read = std::cmp::min(remaining, (chunksize - chunk_off) as usize);

            if chunk_idx >= nchunks {
                break;
            }

            let ci = Self::chunk_index_at(ci_data, chunk_idx);
            let blkaddr = ci.blkaddr();
            if blkaddr == u64::MAX {
                // Hole — zeros
            } else if ci.device_id() > 0 {
                let blob_offset =
                    (blkaddr * EROFS_BLOCK_SIZE as u64 + chunk_off) as i64;
                let data = self.blob_pread_async(to_read, blob_offset).await?;
                result[buf_pos..buf_pos + to_read].copy_from_slice(&data);
            } else {
                let data_offset =
                    (blkaddr * EROFS_BLOCK_SIZE as u64 + chunk_off) as usize;
                let slice = self.mmap_slice(data_offset, to_read)?;
                result[buf_pos..buf_pos + to_read].copy_from_slice(slice);
            }

            file_pos += to_read as u64;
            buf_pos += to_read;
            remaining -= to_read;
        }

        Ok(result)
    }

    // ------------------------------------------------------------------
    // Blob pread (sync + async)
    // ------------------------------------------------------------------

    fn blob_pread_sync(&self, buf: &mut [u8], offset: i64) -> io::Result<()> {
        let fd = self.blob_fd.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "blob device not available")
        })?;
        let mut total = 0;
        while total < buf.len() {
            let ret = unsafe {
                libc::pread(
                    fd.as_raw_fd(),
                    buf[total..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - total,
                    offset + total as i64,
                )
            };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
            if ret == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "blob pread returned 0",
                ));
            }
            total += ret as usize;
        }
        Ok(())
    }

    async fn blob_pread_async(&self, len: usize, offset: i64) -> io::Result<Vec<u8>> {
        let fd = self.blob_fd.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "blob device not available")
        })?;
        let raw_fd = fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            let mut total = 0;
            while total < len {
                let ret = unsafe {
                    libc::pread(
                        raw_fd,
                        buf[total..].as_mut_ptr() as *mut libc::c_void,
                        len - total,
                        offset + total as i64,
                    )
                };
                if ret < 0 {
                    return Err(io::Error::last_os_error());
                }
                if ret == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "blob pread returned 0",
                    ));
                }
                total += ret as usize;
            }
            Ok(buf)
        })
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    }
}
