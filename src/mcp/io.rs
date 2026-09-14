//! Nonblocking pipe/socket I/O. No buffered writer can hide a blocking flush.
use anyhow::{bail, Context, Result};
use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd},
};

pub(super) struct Nonblocking<'a> {
    fd: BorrowedFd<'a>,
    original: libc::c_int,
    restored: bool,
}

impl<'a> Nonblocking<'a> {
    pub(super) fn new(fd: BorrowedFd<'a>) -> Result<Self> {
        // O_NONBLOCK does not bound regular-file/device I/O. MCP stdio must be
        // pipes or sockets; refuse other descriptors instead of promising a deadline.
        // SAFETY: stat is valid output storage and fd remains borrowed throughout.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat writes only to stat, using a live borrowed descriptor.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error()).context("inspecting MCP descriptor");
        }
        if !matches!(stat.st_mode & libc::S_IFMT, libc::S_IFIFO | libc::S_IFSOCK) {
            bail!("MCP stdio requires pipes or sockets for bounded I/O");
        }
        // SAFETY: F_GETFL takes no third argument and does not retain the descriptor.
        let original = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if original == -1 {
            return Err(io::Error::last_os_error()).context("reading MCP descriptor flags");
        }
        // SAFETY: F_SETFL takes these integer flags; fd is still valid.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, original | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error()).context("setting nonblocking MCP I/O");
        }
        Ok(Self {
            fd,
            original,
            restored: false,
        })
    }

    pub(super) fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        // SAFETY: bytes describes writable storage for its entire length; read
        // neither retains the pointer nor closes the borrowed descriptor.
        let size =
            unsafe { libc::read(self.fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        if size < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(size as usize)
        }
    }

    pub(super) fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        // SAFETY: bytes is valid for reads of its length; write retains nothing.
        let size = unsafe { libc::write(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if size < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(size as usize)
        }
    }

    pub(super) fn restore(&mut self) -> Result<()> {
        if !self.restored {
            // SAFETY: restoring flags on the same still-borrowed descriptor.
            if unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, self.original) } == -1 {
                return Err(io::Error::last_os_error()).context("restoring MCP descriptor flags");
            }
            self.restored = true;
        }
        Ok(())
    }
}

impl Drop for Nonblocking<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            crate::warn(format!("MCP descriptor cleanup failed: {error:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::{fd::AsFd, unix::net::UnixStream};

    fn flags(fd: BorrowedFd<'_>) -> libc::c_int {
        // SAFETY: F_GETFL only inspects the live borrowed descriptor.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        flags
    }

    #[test]
    fn restores_original_flags_even_when_opening_output_fails() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        for was_nonblocking in [false, true] {
            stream.set_nonblocking(was_nonblocking).unwrap();
            let original = flags(stream.as_fd());
            {
                let _guard = Nonblocking::new(stream.as_fd()).unwrap();
                assert_ne!(flags(stream.as_fd()) & libc::O_NONBLOCK, 0);
            }
            assert_eq!(flags(stream.as_fd()), original);
            let root = tempfile::tempdir().unwrap();
            let file = tempfile::tempfile().unwrap();
            let error =
                super::super::session::serve(stream.as_fd(), file.as_fd(), root.path().to_owned())
                    .unwrap_err();
            assert!(format!("{error:#}").contains("pipes or sockets"));
            assert_eq!(flags(stream.as_fd()), original);
        }
    }
}
