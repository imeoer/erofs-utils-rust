// erofs-fuse — mount an EROFS image via FUSE using fuse-backend-rs async mode.
//
// Usage:
//   sudo erofs-fuse <image> <mountpoint> [--blobdev <path>]

use std::io::{Error, Result};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use log::{error, info, warn, LevelFilter};
use signal_hook::consts::TERM_SIGNALS;
use signal_hook::iterator::Signals;
use simple_logger::SimpleLogger;

use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::async_util::AsyncExecutorState;
use fuse_backend_rs::transport::{FuseDevTask, FuseSession};

use mkfs_erofs::fs::{ErofsFs, ErofsReader};

#[derive(Parser)]
#[command(name = "erofs-fuse", about = "Mount an EROFS image via FUSE")]
struct Args {
    /// EROFS image file
    image: String,

    /// Mount point
    mountpoint: String,

    /// Optional blob device for chunk-based files
    #[arg(long)]
    blobdev: Option<String>,

    /// Number of worker threads (default: 4)
    #[arg(long, default_value_t = 4)]
    threads: u32,

    /// Filesystem name shown in /proc/mounts SOURCE column
    #[arg(long, default_value = "erofs-fuse")]
    fsname: String,
}

fn main() -> Result<()> {
    SimpleLogger::new()
        .with_level(LevelFilter::Trace)
        .init()
        .unwrap();

    let args = Args::parse();

    let mountpoint = Path::new(&args.mountpoint);
    if !mountpoint.is_dir() {
        error!("mountpoint {} is not a directory", args.mountpoint);
        return Err(Error::from_raw_os_error(libc::EINVAL));
    }

    // ErofsReader::open() is async — use a temporary tokio runtime for initialization.
    let reader = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::new(std::io::ErrorKind::Other, e))?;
        rt.block_on(ErofsReader::open(&args.image, args.blobdev.as_deref()))?
    };
    info!(
        "opened EROFS image: root_nid={}, blocks={}, inos={}",
        reader.sb().root_nid(),
        reader.sb().blocks(),
        reader.sb().inos()
    );

    let fs = ErofsFs::new(Arc::new(reader));
    let server = Arc::new(Server::new(fs));

    let mut se = FuseSession::new(mountpoint, &args.fsname, "", true)
        .map_err(|e| Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
    se.mount()
        .map_err(|e| Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
    info!("mounted on {}", args.mountpoint);

    let buf_size = se.bufsize();
    let state = AsyncExecutorState::new();

    for i in 0..args.threads {
        let fuse_file = se
            .clone_fuse_file()
            .map_err(|e| Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
        let fd = fuse_file.as_raw_fd();
        let server = server.clone();
        let state = state.clone();

        std::thread::Builder::new()
            .name(format!("erofs_fuse_{}", i))
            .spawn(move || {
                let _fuse_file = fuse_file;
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async move {
                    let mut task = FuseDevTask::new(buf_size, fd, server, state);
                    info!("fuse worker {} started", i);
                    task.poll_handler().await;
                    warn!("fuse worker {} exits", i);
                });
            })
            .map_err(|e| Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
    }

    // Wait for termination signals
    let mut signals =
        Signals::new(TERM_SIGNALS).map_err(|e| Error::new(std::io::ErrorKind::Other, e))?;
    for _sig in signals.forever() {
        break;
    }

    info!("unmounting...");
    state.quiesce();
    se.umount()
        .map_err(|e| Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
    se.wake()
        .map_err(|e| Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;

    Ok(())
}
