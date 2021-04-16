use futures_util::ready;
use std::{
    fs::OpenOptions,
    io::{Error, Result},
    os::unix::{
        io::{AsRawFd, IntoRawFd, RawFd},
        prelude::OpenOptionsExt,
    },
    task::Poll,
};
use tokio::io::{unix::AsyncFd, AsyncRead};

pub(crate) struct Stdin(AsyncFd<RawFd>);

impl Stdin {
    pub(crate) fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/stdin")?;
        Ok(Self(AsyncFd::new(file.into_raw_fd())?))
    }
}

impl AsRawFd for Stdin {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsyncRead for Stdin {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<Result<()>> {
        loop {
            let mut ready = ready!(self.0.poll_read_ready(cx))?;
            match ready.try_io(|inner| read(inner.as_raw_fd(), buf)) {
                Ok(res) => return Poll::Ready(res),
                Err(_would_block) => continue,
            }
        }
    }
}

fn read(fd: RawFd, buf: &mut tokio::io::ReadBuf<'_>) -> Result<()> {
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
