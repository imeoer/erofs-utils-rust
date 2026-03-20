# erofs-utils-rust

A minimal Rust implementation of `mkfs.erofs` focused on chunk-based image creation.

## Build

```bash
cargo build --release
```

The generated binary is:

```bash
./target/release/mkfs-erofs
```

## Usage

Command format:

```bash
./target/release/mkfs-erofs IMAGE --blobdev BLOB_IMAGE --chunksize BYTES SOURCE
```

Arguments:

- `IMAGE`: output EROFS metadata image
- `--blobdev`: output blob data file
- `--chunksize`: chunk size in bytes, must be a power of two and at least 4096
- `SOURCE`: source directory

Example:

```bash
./target/release/mkfs-erofs /tmp/erofs.meta.img \
	--blobdev /tmp/erofs.blob.img \
	--chunksize 1048576 \
	~/code/linux
```

## Current Scope

This version currently implements only a minimal subset:

- `--blobdev`
- `--chunksize`
- directory input as `SOURCE`

Not supported:

- tar/OCI/S3 inputs
- compression options
- xattr and SELinux related options
- incremental builds
