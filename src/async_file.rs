// Copyright (C) 2022 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Debug, Formatter};
use std::io::{ErrorKind, IoSlice, IoSliceMut};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;

use crate::async_runtime::{Runtime, CURRENT_RUNTIME};
use crate::buf::FileVolatileBuf;
use crate::{off64_t, preadv64, pwritev64};

/// An adapter enum to support both tokio and tokio-uring asynchronous `File`.
pub enum File {
    /// Tokio asynchronous `File`.
    Tokio(tokio::fs::File),
    #[cfg(target_os = "linux")]
    /// Tokio-uring asynchronous `File`.
    ///
    /// `tokio_uring::fs::File` is !Send, and it breaks all async functions because it's !Send.
    /// On the other hand, tokio_uring::fs::File could only be used with tokio current thread
    /// Runtime, which means it will never be sent. So play a trick here to make it works with
    /// async functions.
    Uring(usize),
}

impl File {
    /// Asynchronously open a file.
    pub async fn async_open<P: AsRef<Path>>(
        path: P,
        write: bool,
        create: bool,
    ) -> std::io::Result<Self> {
        let ty = CURRENT_RUNTIME.with(|rt| match rt {
            Runtime::Tokio(_) => 1,
            #[cfg(target_os = "linux")]
            Runtime::Uring => 2,
        });

        match ty {
            1 => tokio::fs::OpenOptions::new()
                .read(true)
                .write(write)
                .create(create)
                .open(path)
                .await
                .map(File::Tokio),
            #[cfg(target_os = "linux")]
            2 => tokio_uring::fs::OpenOptions::new()
                .read(true)
                .write(write)
                .create(create)
                .open(path)
                .await
                .map(|v| File::Uring(Box::into_raw(Box::new(v)) as usize)),
            _ => panic!("should not happen"),
        }
    }

    /// Asynchronously read data at `offset` into the buffer.
    pub async fn async_read_at(
        &self,
        buf: FileVolatileBuf,
        offset: u64,
    ) -> (std::io::Result<usize>, FileVolatileBuf) {
        match self {
            File::Tokio(f) => {
                // tokio::fs:File doesn't support read_at() yet.
                //f.read_at(buf, offset).await,
                let mut bufs = [buf];
                let res = preadv(f.as_raw_fd(), &mut bufs, offset);
                (res, bufs[0])
            }
            #[cfg(target_os = "linux")]
            File::Uring(_) => self.as_tokio_uring_file().read_at(buf, offset as u64).await,
        }
    }

    /// Asynchronously read data at `offset` into buffers.
    pub async fn async_readv_at(
        &self,
        mut bufs: Vec<FileVolatileBuf>,
        offset: u64,
    ) -> (std::io::Result<usize>, Vec<FileVolatileBuf>) {
        match self {
            File::Tokio(f) => {
                // tokio::fs:File doesn't support read_at() yet.
                //f.read_at(buf, offset).await,
                let res = preadv(f.as_raw_fd(), &mut bufs, offset);
                (res, bufs)
            }
            #[cfg(target_os = "linux")]
            File::Uring(_) => {
                // TODO: enhance tokio-uring to support readv_at
                let file = self.as_tokio_uring_file();
                let res = preadv(file.as_raw_fd(), &mut bufs, offset);
                (res, bufs)
            }
        }
    }

    /// Asynchronously write data at `offset` from the buffer.
    pub async fn async_write_at(
        &self,
        buf: FileVolatileBuf,
        offset: u64,
    ) -> (std::io::Result<usize>, FileVolatileBuf) {
        match self {
            File::Tokio(f) => {
                // tokio::fs:File doesn't support read_at() yet.
                //f.read_at(buf, offset).await,
                let bufs = [buf];
                let res = pwritev(f.as_raw_fd(), &bufs, offset);
                (res, bufs[0])
            }
            #[cfg(target_os = "linux")]
            File::Uring(_) => {
                self.as_tokio_uring_file()
                    .write_at(buf, offset as u64)
                    .await
            }
        }
    }

    /// Asynchronously write data at `offset` from buffers.
    pub async fn async_writev_at(
        &self,
        bufs: Vec<FileVolatileBuf>,
        offset: u64,
    ) -> (std::io::Result<usize>, Vec<FileVolatileBuf>) {
        match self {
            File::Tokio(f) => {
                // tokio::fs:File doesn't support read_at() yet.
                //f.read_at(buf, offset).await,
                let res = pwritev(f.as_raw_fd(), &bufs, offset);
                (res, bufs)
            }
            #[cfg(target_os = "linux")]
            File::Uring(_) => {
                // TODO: enhance tokio-uring to support writev_at
                let file = self.as_tokio_uring_file();
                let res = pwritev(file.as_raw_fd(), &bufs, offset);
                (res, bufs)
            }
        }
    }

    /// Get metadata about the file.
    pub fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        let file = match self {
            File::Tokio(f) => {
                // Safe because we have manually forget() the `file` object below.
                unsafe { std::fs::File::from_raw_fd(f.as_raw_fd()) }
            }
            #[cfg(target_os = "linux")]
            File::Uring(_) => {
                // Safe because we have manually forget() the `file` object below.
                let f = self.as_tokio_uring_file();
                unsafe { std::fs::File::from_raw_fd(f.as_raw_fd()) }
            }
        };
        let res = file.metadata();
        std::mem::forget(file);
        res
    }

    /// Try to clone the file object.
    pub async fn async_try_clone(&self) -> std::io::Result<Self> {
        match self {
            File::Tokio(f) => f.try_clone().await.map(File::Tokio),
            #[cfg(target_os = "linux")]
            // TODO
            File::Uring(_f) => unimplemented!(),
        }
    }

    // Convert back to an tokio_uring::fs::File object.
    //
    // # Panic
    // Panics if `self` is not a `tokio_uring::fs::File` object.
    #[cfg(target_os = "linux")]
    fn as_tokio_uring_file(&self) -> &tokio_uring::fs::File {
        if let File::Uring(v) = self {
            // Safe because `v` is a raw pointer to a `tokio_uring::fs::File` object.
            unsafe { &*(*v as *const tokio_uring::fs::File) }
        } else {
            panic!("as_tokio_uring_file() should only be called for toki_uring::fs::File objects");
        }
    }
}

impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            File::Tokio(f) => f.as_raw_fd(),
            #[cfg(target_os = "linux")]
            File::Uring(_) => self.as_tokio_uring_file().as_raw_fd(),
        }
    }
}

impl Debug for File {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let fd = self.as_raw_fd();
        write!(f, "Async File {}", fd)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let File::Uring(f) = self {
            // Safe because it's paired with Box::into_raw() in File::new().
            drop(unsafe { Box::from_raw(*f as *mut tokio_uring::fs::File) });
        }
    }
}

/// A simple wrapper over posix `preadv` to deal with `FileVolatileBuf`.
pub fn preadv(fd: RawFd, bufs: &mut [FileVolatileBuf], offset: u64) -> std::io::Result<usize> {
    let iov: Vec<IoSliceMut> = bufs.iter().map(|v| v.io_slice_mut()).collect();

    loop {
        // SAFETY: it is ABI compatible, a pointer cast here is valid
        let res = unsafe {
            preadv64(
                fd,
                iov.as_ptr() as *const libc::iovec,
                iov.len() as libc::c_int,
                offset as off64_t,
            )
        };

        if res >= 0 {
            let mut count = res as usize;
            for buf in bufs.iter_mut() {
                let cnt = std::cmp::min(count, buf.cap() - buf.len());
                unsafe { buf.set_size(buf.len() + cnt) };
                count -= cnt;
                if count == 0 {
                    break;
                }
            }
            assert_eq!(count, 0);
            return Ok(res as usize);
        } else {
            let e = std::io::Error::last_os_error();
            // Retry if the IO is interrupted by signal.
            if e.kind() != ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// A simple wrapper over posix `pwritev` to deal with `FileVolatileBuf`.
pub fn pwritev(fd: RawFd, bufs: &[FileVolatileBuf], offset: u64) -> std::io::Result<usize> {
    let iov: Vec<IoSlice> = bufs.iter().map(|v| v.io_slice()).collect();

    loop {
        // SAFETY: it is ABI compatible, a pointer cast here is valid
        let res = unsafe {
            pwritev64(
                fd,
                iov.as_ptr() as *const libc::iovec,
                iov.len() as libc::c_int,
                offset as off64_t,
            )
        };

        if res >= 0 {
            return Ok(res as usize);
        } else {
            let e = std::io::Error::last_os_error();
            // Retry if the IO is interrupted by signal.
            if e.kind() != ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_runtime::block_on;
    use vmm_sys_util::tempdir::TempDir;

    #[test]
    fn test_new_async_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().to_path_buf().join("test.txt");
        std::fs::write(&path, b"test").unwrap();

        let file = block_on(async { File::async_open(&path, false, false).await.unwrap() });
        assert!(file.as_raw_fd() >= 0);
        drop(file);
    }

    #[test]
    fn test_async_file_metadata() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().to_path_buf();
        std::fs::write(path.join("test.txt"), b"test").unwrap();
        let file = block_on(async {
            File::async_open(path.join("test.txt"), false, false)
                .await
                .unwrap()
        });

        let md = file.metadata().unwrap();
        assert!(md.is_file());
        let md = file.metadata().unwrap();
        assert!(md.is_file());

        drop(file);
    }

    #[test]
    fn test_async_read_at() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().to_path_buf();
        std::fs::write(path.join("test.txt"), b"test").unwrap();

        block_on(async {
            let file = File::async_open(path.join("test.txt"), false, false)
                .await
                .unwrap();

            let mut buffer = [0u8; 3];
            let buf = unsafe { FileVolatileBuf::new(&mut buffer) };
            let (res, buf) = file.async_read_at(buf, 0).await;
            assert_eq!(res.unwrap(), 3);
            assert_eq!(buf.len(), 3);
            let buf = unsafe { FileVolatileBuf::new(&mut buffer) };
            let (res, buf) = file.async_read_at(buf, 2).await;
            assert_eq!(res.unwrap(), 2);
            assert_eq!(buf.len(), 2);
        });
    }

    #[test]
    fn test_async_readv_at() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().to_path_buf();
        std::fs::write(path.join("test.txt"), b"test").unwrap();

        block_on(async {
            let file = File::async_open(path.join("test.txt"), false, false)
                .await
                .unwrap();

            let mut buffer = [0u8; 3];
            let buf = unsafe { FileVolatileBuf::new(&mut buffer) };
            let mut buffer2 = [0u8; 3];
            let buf2 = unsafe { FileVolatileBuf::new(&mut buffer2) };
            let bufs = vec![buf, buf2];
            let (res, bufs) = file.async_readv_at(bufs, 0).await;

            assert_eq!(res.unwrap(), 4);
            assert_eq!(bufs[0].len(), 3);
            assert_eq!(bufs[1].len(), 1);
        });
    }

    #[test]
    fn test_async_write_at() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().to_path_buf();

        block_on(async {
            let file = File::async_open(path.join("test.txt"), true, true)
                .await
                .unwrap();

            let buffer = b"test";
            let buf = unsafe {
                FileVolatileBuf::from_raw(buffer.as_ptr() as *mut u8, buffer.len(), buffer.len())
            };
            let (res, buf) = file.async_write_at(buf, 0).await;
            assert_eq!(res.unwrap(), 4);
            assert_eq!(buf.len(), 4);

            let res = std::fs::read_to_string(path.join("test.txt")).unwrap();
            assert_eq!(&res, "test");
        });
    }

    #[test]
    fn test_async_writev_at() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().to_path_buf();

        block_on(async {
            let file = File::async_open(path.join("test.txt"), true, true)
                .await
                .unwrap();

            let buffer = b"tes";
            let buf = unsafe {
                FileVolatileBuf::from_raw(buffer.as_ptr() as *mut u8, buffer.len(), buffer.len())
            };
            let buffer2 = b"t";
            let buf2 = unsafe {
                FileVolatileBuf::from_raw(buffer2.as_ptr() as *mut u8, buffer2.len(), buffer2.len())
            };
            let bufs = vec![buf, buf2];
            let (res, bufs) = file.async_writev_at(bufs, 0).await;

            assert_eq!(res.unwrap(), 4);
            assert_eq!(bufs[0].len(), 3);
            assert_eq!(bufs[1].len(), 1);

            let res = std::fs::read_to_string(path.join("test.txt")).unwrap();
            assert_eq!(&res, "test");
        });
    }
}