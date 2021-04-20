use crate::{Error, IntoStd, Result};
use futures_util::ready;
use nix::{
    fcntl::{self, FcntlArg, OFlag},
    libc, pty, unistd,
};
use std::{
    os::unix::{
        io::{AsRawFd, RawFd},
        prelude::IntoRawFd,
    },
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf};

#[derive(Debug)]
pub struct PtyMaster {
    master_fd: AsyncFd<RawFd>,
    slave_name: String,
}

impl PtyMaster {
    pub fn open() -> Result<Self> {
        let flags = OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
        let master_fd = pty::posix_openpt(flags).into_std()?;
        pty::grantpt(&master_fd).into_std()?;
        pty::unlockpt(&master_fd).into_std()?;

        let slave_name = pty::ptsname_r(&master_fd).into_std()?;
        let master_fd = AsyncFd::new(master_fd.into_raw_fd())?;

        Ok(Self {
            master_fd,
            slave_name,
        })
    }

    pub fn slave_name(&self) -> &str {
        &self.slave_name
    }

    pub fn try_clone(&self) -> Result<Self> {
        let fd = fcntl::fcntl(self.as_raw_fd(), FcntlArg::F_DUPFD_CLOEXEC(0)).into_std()?;

        let master_fd = AsyncFd::new(fd)?;
        let slave_name = self.slave_name.clone();

        Ok(Self {
            master_fd,
            slave_name,
        })
    }
}

impl AsRawFd for PtyMaster {
    fn as_raw_fd(&self) -> RawFd {
        self.master_fd.as_raw_fd()
    }
}

impl AsyncRead for PtyMaster {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        loop {
            let mut ready = ready!(self.master_fd.poll_read_ready(cx))?;
            match ready.try_io(|inner| read(inner.as_raw_fd(), buf)) {
                Ok(res) => return Poll::Ready(res),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for PtyMaster {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        loop {
            let mut ready = ready!(self.master_fd.poll_write_ready(cx))?;
            match ready.try_io(|inner| write(inner.as_raw_fd(), buf)) {
                Ok(res) => return Poll::Ready(res),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn read(fd: RawFd, buf: &mut ReadBuf<'_>) -> Result<()> {
    let ret = unsafe { libc::read(fd, buf.unfilled_mut() as *mut _ as _, buf.remaining()) };
    if ret < 0 {
        return Err(Error::last_os_error());
    }
    let n = ret as usize;
    unsafe {
        buf.assume_init(n);
    }
    buf.advance(n);
    Ok(())
}

fn write(fd: RawFd, buf: &[u8]) -> Result<usize> {
    unistd::write(fd, buf).into_std()
}
