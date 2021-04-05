use std::{
    fs::OpenOptions,
    io::{Error, ErrorKind, Result},
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
            let mut ready = match self.0.poll_read_ready(cx) {
                Poll::Ready(x) => x?,
                Poll::Pending => return Poll::Pending,
            };

            let ret = unsafe {
                libc::read(
                    self.as_raw_fd(),
                    buf.unfilled_mut() as *mut _ as _,
                    buf.remaining(),
                )
            };

            if ret < 0 {
                let e = Error::last_os_error();
                if e.kind() == ErrorKind::WouldBlock {
                    ready.clear_ready();
                    continue;
                }
                return Poll::Ready(Err(e));
            }

            let n = ret as usize;
            unsafe {
                buf.assume_init(n);
            }
            buf.advance(n);
            return Poll::Ready(Ok(()));
        }
    }
}
