use crate::prelude::*;
use parking_lot::Mutex;
use std::{io::Write, panic};

static RAW_MODE: Mutex<imp::RawMode> = Mutex::const_new(
    parking_lot::lock_api::RawMutex::INIT,
    imp::RawMode::new(libc::STDIN_FILENO),
);

pub(crate) fn enter() -> Result<bool> {
    trace!("enter raw mode");
    let entered = RAW_MODE.lock().enter()?;
    Ok(entered)
}

pub(crate) fn enter_scoped() -> Result<RawModeGuard> {
    enter()?;
    Ok(RawModeGuard {})
}

pub(crate) fn leave() -> Result<bool> {
    let left = RAW_MODE.lock().leave()?;
    trace!("leave raw mode");
    Ok(left)
}

pub(crate) fn leave_on_panic() {
    let saved_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let left = RAW_MODE
            .lock()
            .leave()
            .expect("failed to restore terminal mode");
        if left {
            debug!("escape from raw mode");
        }
        saved_hook(info);
    }));
}

#[must_use]
#[derive(Debug)]
pub(crate) struct RawModeGuard {}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        leave().expect("failed to restore terminal mode");
    }
}

#[derive(Debug)]
pub(crate) struct RawModeWriter<W> {
    inner: W,
}

impl<W> RawModeWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W> Write for RawModeWriter<W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Acquire RAW_MODE lock to avoid race condition / output corruption
        let raw_mode = RAW_MODE.lock();

        let mut converted = vec![];
        let out = if raw_mode.is_raw_mode() {
            converted.reserve(buf.len());
            // convert "\n" -> "\r\n"
            for (idx, line) in buf.split(|ch| *ch == b'\n').enumerate() {
                if idx > 0 {
                    converted.extend_from_slice(&[b'\r', b'\n']);
                }
                converted.extend_from_slice(line);
            }
            &converted
        } else {
            buf
        };

        self.inner.write_all(out)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

mod imp {
    use super::*;
    use nix::sys::termios::{self, Termios};
    use std::os::unix::io::RawFd;

    pub(super) struct RawMode {
        fd: RawFd,
        orig: Option<Termios>,
    }

    impl RawMode {
        pub(super) const fn new(fd: RawFd) -> Self {
            Self { fd, orig: None }
        }

        pub(super) fn enter(&mut self) -> Result<bool> {
            if self.orig.is_none() {
                let orig = Some(enter_raw_mode(self.fd)?);
                self.orig = orig;
                Ok(true)
            } else {
                Ok(false)
            }
        }

        pub(super) fn leave(&mut self) -> Result<bool> {
            if let Some(orig) = self.orig.take() {
                leave_raw_mode(self.fd, orig)?;
                Ok(true)
            } else {
                Ok(false)
            }
        }

        pub(crate) fn is_raw_mode(&self) -> bool {
            self.orig.is_some()
        }
    }

    fn enter_raw_mode(fd: RawFd) -> Result<Termios> {
        use termios::SetArg;

        let orig = termios::tcgetattr(fd)?;
        let mut raw = orig.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSAFLUSH, &raw)?;

        Ok(orig)
    }

    fn leave_raw_mode(fd: RawFd, orig: Termios) -> Result<()> {
        use termios::SetArg;

        termios::tcsetattr(fd, SetArg::TCSAFLUSH, &orig)?;

        Ok(())
    }
}
