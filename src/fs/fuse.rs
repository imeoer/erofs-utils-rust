use std::ffi::CStr;
use std::io;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fuse_backend_rs::abi::fuse_abi::{stat64, statvfs64, OpenOptions};
use fuse_backend_rs::api::filesystem::{Context, DirEntry, Entry, FileSystem, ZeroCopyWriter};

#[cfg(feature = "async-io")]
use async_trait::async_trait;

use crate::metadata::*;

use super::{CachedDirEntry, ErofsReader};

const FUSE_ROOT_ID: u64 = 1;
const EROFS_FUSE_TIMEOUT: Duration = Duration::from_secs(86400 * 365 * 10);

pub struct ErofsFs {
    reader: Arc<ErofsReader>,
    dir_handles: Mutex<HashMap<u64, Arc<DirHandle>>>,
    next_dir_handle: AtomicU64,
}

struct DirHandle {
    entries: Vec<CachedDirEntry>,
}

impl ErofsFs {
    pub fn new(reader: Arc<ErofsReader>) -> Self {
        Self {
            reader,
            dir_handles: Mutex::new(HashMap::new()),
            next_dir_handle: AtomicU64::new(1),
        }
    }

    fn to_nid(&self, ino: u64) -> u64 {
        if ino == FUSE_ROOT_ID {
            self.reader.sb().root_nid()
        } else {
            ino - FUSE_ROOT_ID
        }
    }

    fn to_ino(&self, nid: u64) -> u64 {
        if nid == self.reader.sb().root_nid() {
            FUSE_ROOT_ID
        } else {
            nid + FUSE_ROOT_ID
        }
    }

    fn make_entry(&self, nid: u64, inode: &ErofsInode<'_>) -> Entry {
        let ino = self.to_ino(nid);
        Entry {
            inode: ino,
            generation: 0,
            attr: self.make_stat(nid, inode),
            attr_flags: 0,
            attr_timeout: EROFS_FUSE_TIMEOUT,
            entry_timeout: EROFS_FUSE_TIMEOUT,
        }
    }

    fn make_stat(&self, nid: u64, inode: &ErofsInode<'_>) -> stat64 {
        let ino = self.to_ino(nid);
        let sb = self.reader.sb();
        let block_size = 1u64 << sb.blkszbits;
        let mtime = inode.mtime(sb.epoch());
        let mtime_nsec = inode.mtime_nsec();
        let size = inode.size();

        let mut st: stat64 = unsafe { std::mem::zeroed() };
        st.st_ino = ino;
        st.st_mode = inode.mode() as u32;
        st.st_nlink = inode.nlink() as _;
        st.st_size = size as i64;
        st.st_blocks = ((size + block_size - 1) / block_size * block_size / 512) as i64;
        st.st_uid = inode.uid();
        st.st_gid = inode.gid();
        st.st_atime = mtime as i64;
        st.st_mtime = mtime as i64;
        st.st_ctime = mtime as i64;
        st.st_atime_nsec = mtime_nsec as i64;
        st.st_mtime_nsec = mtime_nsec as i64;
        st.st_ctime_nsec = mtime_nsec as i64;
        st.st_blksize = block_size as _;

        let mode = inode.mode() as u32;
        if (mode & libc::S_IFMT) == libc::S_IFCHR || (mode & libc::S_IFMT) == libc::S_IFBLK {
            st.st_rdev = inode.rdev() as u64;
        }

        st
    }

    fn iterate_dir<F>(&self, inode: u64, mut cb: F) -> io::Result<()>
    where
        F: FnMut(u64, u8, &[u8]) -> io::Result<bool>,
    {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        self.reader
            .for_each_dir_entry(nid, &vi, |entry_nid, file_type, name| {
                cb(entry_nid, file_type, name)
            })
    }

    fn create_dir_handle(&self, inode: u64) -> io::Result<u64> {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        let entries = self.reader.read_dir_cached(nid, &vi)?;
        let handle = self.next_dir_handle.fetch_add(1, Ordering::Relaxed);
        let dir_handle = Arc::new(DirHandle { entries });
        self.dir_handles
            .lock()
            .unwrap()
            .insert(handle, dir_handle);
        Ok(handle)
    }

    fn get_dir_handle(&self, handle: u64) -> io::Result<Arc<DirHandle>> {
        self.dir_handles
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))
    }
}

impl FileSystem for ErofsFs {
    type Inode = u64;
    type Handle = u64;

    fn init(
        &self,
        capable: fuse_backend_rs::abi::fuse_abi::FsOptions,
    ) -> io::Result<fuse_backend_rs::abi::fuse_abi::FsOptions> {
        use fuse_backend_rs::abi::fuse_abi::FsOptions;

        // Request all capabilities that benefit a read-only filesystem.
        let want = FsOptions::ASYNC_READ       // allow parallel reads
            | FsOptions::BIG_WRITES            // enable large max_write (1MB)
            | FsOptions::MAX_PAGES             // use 256 pages (1MB) for max_write/max_read
            | FsOptions::PARALLEL_DIROPS       // parallel directory operations
            | FsOptions::DO_READDIRPLUS        // READDIRPLUS support
            | FsOptions::READDIRPLUS_AUTO      // auto-READDIRPLUS
            | FsOptions::ASYNC_DIO             // async direct I/O
            | FsOptions::CACHE_SYMLINKS; // cache symlink targets

        // Negotiate: only enable what the kernel also supports.
        Ok(capable & want)
    }

    fn destroy(&self) {}

    fn lookup(&self, _ctx: &Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        let target = name.to_bytes();
        let mut found = None;
        self.iterate_dir(parent, |entry_nid, _file_type, entry_name| {
            if entry_name == target {
                found = Some(entry_nid);
                return Ok(false);
            }
            Ok(true)
        })?;

        if let Some(child_nid) = found {
            let child_inode = self.reader.inode(child_nid)?;
            return Ok(self.make_entry(child_nid, &child_inode));
        }

        Err(io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn forget(&self, _ctx: &Context, _inode: u64, _count: u64) {}

    fn getattr(
        &self,
        _ctx: &Context,
        inode: u64,
        _handle: Option<u64>,
    ) -> io::Result<(stat64, Duration)> {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        Ok((self.make_stat(nid, &vi), EROFS_FUSE_TIMEOUT))
    }

    fn open(
        &self,
        _ctx: &Context,
        inode: u64,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions, Option<u32>)> {
        if flags & (libc::O_WRONLY as u32 | libc::O_RDWR as u32) != 0 {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }

        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        if (vi.mode() as u32 & libc::S_IFMT) != libc::S_IFREG {
            return Err(io::Error::from_raw_os_error(libc::EISDIR));
        }

        Ok((Some(nid), OpenOptions::KEEP_CACHE, None))
    }

    fn release(
        &self,
        _ctx: &Context,
        _inode: u64,
        _flags: u32,
        _handle: u64,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn read(
        &self,
        _ctx: &Context,
        inode: u64,
        _handle: u64,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        self.reader.write_file_data_to(nid, &vi, offset, size, w)
    }

    fn readlink(&self, _ctx: &Context, inode: u64) -> io::Result<Vec<u8>> {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        self.reader.read_symlink(nid, &vi)
    }

    fn opendir(
        &self,
        _ctx: &Context,
        inode: u64,
        _flags: u32,
    ) -> io::Result<(Option<u64>, OpenOptions)> {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        if (vi.mode() as u32 & libc::S_IFMT) != libc::S_IFDIR {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }

        let handle = self.create_dir_handle(inode)?;
        Ok((Some(handle), OpenOptions::CACHE_DIR | OpenOptions::KEEP_CACHE))
    }

    fn readdir(
        &self,
        _ctx: &Context,
        _inode: u64,
        handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let dir_handle = self.get_dir_handle(handle)?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (idx, entry) in dir_handle.entries.iter().enumerate().skip(start) {
            let dir_entry = DirEntry {
                ino: self.to_ino(entry.nid),
                offset: idx as u64 + 1,
                type_: erofs_ft_to_dt(entry.file_type),
                name: &entry.name,
            };

            match add_entry(dir_entry) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        _ctx: &Context,
        _inode: u64,
        handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let dir_handle = self.get_dir_handle(handle)?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (idx, entry) in dir_handle.entries.iter().enumerate().skip(start) {
            let child_inode = self.reader.inode(entry.nid)?;
            let fuse_entry = self.make_entry(entry.nid, &child_inode);
            let dir_entry = DirEntry {
                ino: self.to_ino(entry.nid),
                offset: idx as u64 + 1,
                type_: erofs_ft_to_dt(entry.file_type),
                name: &entry.name,
            };

            match add_entry(dir_entry, fuse_entry) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn releasedir(&self, _ctx: &Context, _inode: u64, _flags: u32, handle: u64) -> io::Result<()> {
        self.dir_handles.lock().unwrap().remove(&handle);
        Ok(())
    }

    fn statfs(&self, _ctx: &Context, _inode: u64) -> io::Result<statvfs64> {
        let sb = self.reader.sb();
        let block_size = 1u64 << sb.blkszbits;
        let mut st: statvfs64 = unsafe { std::mem::zeroed() };
        st.f_bsize = block_size;
        st.f_frsize = block_size;
        st.f_blocks = sb.blocks();
        st.f_files = sb.inos();
        st.f_namemax = 255;
        Ok(st)
    }

    fn access(&self, _ctx: &Context, _inode: u64, _mask: u32) -> io::Result<()> {
        Ok(())
    }

    fn getxattr(
        &self,
        _ctx: &Context,
        inode: u64,
        name: &CStr,
        size: u32,
    ) -> io::Result<fuse_backend_rs::api::filesystem::GetxattrReply> {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        let name_bytes = name.to_bytes();

        let xattrs = self.reader.read_xattrs(nid, &vi)?;
        for (xname, xvalue) in &xattrs {
            if xname.as_slice() == name_bytes {
                if size == 0 {
                    return Ok(fuse_backend_rs::api::filesystem::GetxattrReply::Count(
                        xvalue.len() as u32,
                    ));
                }
                if (size as usize) < xvalue.len() {
                    return Err(io::Error::from_raw_os_error(libc::ERANGE));
                }
                return Ok(fuse_backend_rs::api::filesystem::GetxattrReply::Value(
                    xvalue.clone(),
                ));
            }
        }

        Err(io::Error::from_raw_os_error(libc::ENODATA))
    }

    fn listxattr(
        &self,
        _ctx: &Context,
        inode: u64,
        size: u32,
    ) -> io::Result<fuse_backend_rs::api::filesystem::ListxattrReply> {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        let xattrs = self.reader.read_xattrs(nid, &vi)?;

        // Build null-separated list of xattr names
        let mut names_buf: Vec<u8> = Vec::new();
        for (xname, _) in &xattrs {
            names_buf.extend_from_slice(xname);
            names_buf.push(0);
        }

        if size == 0 {
            return Ok(fuse_backend_rs::api::filesystem::ListxattrReply::Count(
                names_buf.len() as u32,
            ));
        }
        if (size as usize) < names_buf.len() {
            return Err(io::Error::from_raw_os_error(libc::ERANGE));
        }
        Ok(fuse_backend_rs::api::filesystem::ListxattrReply::Names(
            names_buf,
        ))
    }
}

#[cfg(feature = "async-io")]
#[async_trait]
impl fuse_backend_rs::api::filesystem::AsyncFileSystem for ErofsFs {
    async fn async_lookup(
        &self,
        ctx: &Context,
        parent: <Self as FileSystem>::Inode,
        name: &CStr,
    ) -> io::Result<Entry> {
        self.lookup(ctx, parent, name)
    }

    async fn async_getattr(
        &self,
        ctx: &Context,
        inode: <Self as FileSystem>::Inode,
        handle: Option<<Self as FileSystem>::Handle>,
    ) -> io::Result<(stat64, Duration)> {
        self.getattr(ctx, inode, handle)
    }

    async fn async_setattr(
        &self,
        _ctx: &Context,
        _inode: <Self as FileSystem>::Inode,
        _attr: stat64,
        _handle: Option<<Self as FileSystem>::Handle>,
        _valid: fuse_backend_rs::abi::fuse_abi::SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    async fn async_open(
        &self,
        ctx: &Context,
        inode: <Self as FileSystem>::Inode,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<(Option<<Self as FileSystem>::Handle>, OpenOptions)> {
        self.open(ctx, inode, flags, fuse_flags)
            .map(|(h, o, _)| (h, o))
    }

    async fn async_create(
        &self,
        _ctx: &Context,
        _parent: <Self as FileSystem>::Inode,
        _name: &CStr,
        _args: fuse_backend_rs::abi::fuse_abi::CreateIn,
    ) -> io::Result<(Entry, Option<<Self as FileSystem>::Handle>, OpenOptions)> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    async fn async_read(
        &self,
        _ctx: &Context,
        inode: <Self as FileSystem>::Inode,
        _handle: <Self as FileSystem>::Handle,
        w: &mut (dyn fuse_backend_rs::api::filesystem::AsyncZeroCopyWriter + Send),
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let nid = self.to_nid(inode);
        let vi = self.reader.inode(nid)?;
        // Use sync read path directly — each worker runs a single-threaded
        // tokio runtime with only one FUSE task, so blocking is equivalent to
        // awaiting and avoids spawn_blocking overhead (thread pool dispatch,
        // cross-thread channel, Vec allocation per chunk).
        let data = self.reader.read_file_data_sync(nid, &vi, offset, size)?;
        w.write(&data)
    }

    async fn async_write(
        &self,
        _ctx: &Context,
        _inode: <Self as FileSystem>::Inode,
        _handle: <Self as FileSystem>::Handle,
        _r: &mut (dyn fuse_backend_rs::api::filesystem::AsyncZeroCopyReader + Send),
        _size: u32,
        _offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    async fn async_fsync(
        &self,
        _ctx: &Context,
        _inode: <Self as FileSystem>::Inode,
        _datasync: bool,
        _handle: <Self as FileSystem>::Handle,
    ) -> io::Result<()> {
        Ok(())
    }

    async fn async_fallocate(
        &self,
        _ctx: &Context,
        _inode: <Self as FileSystem>::Inode,
        _handle: <Self as FileSystem>::Handle,
        _mode: u32,
        _offset: u64,
        _length: u64,
    ) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    async fn async_fsyncdir(
        &self,
        _ctx: &Context,
        _inode: <Self as FileSystem>::Inode,
        _datasync: bool,
        _handle: <Self as FileSystem>::Handle,
    ) -> io::Result<()> {
        Ok(())
    }
}

fn erofs_ft_to_dt(ft: u8) -> u32 {
    match ft {
        EROFS_FT_REG_FILE => libc::DT_REG as u32,
        EROFS_FT_DIR => libc::DT_DIR as u32,
        EROFS_FT_CHRDEV => libc::DT_CHR as u32,
        EROFS_FT_BLKDEV => libc::DT_BLK as u32,
        EROFS_FT_FIFO => libc::DT_FIFO as u32,
        EROFS_FT_SOCK => libc::DT_SOCK as u32,
        EROFS_FT_SYMLINK => libc::DT_LNK as u32,
        _ => libc::DT_UNKNOWN as u32,
    }
}
