use crate::Result;
use parking_lot::Mutex;

static RAW_MODE: Mutex<imp::RawMode> = Mutex::const_new(
    parking_lot::lock_api::RawMutex::INIT,
    imp::RawMode::new(libc::STDIN_FILENO),
);

pub(crate) struct RawMode {}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.leave().expect("failed to restore terminal mode");
    }
}

impl RawMode {
    pub(crate) fn new() -> Self {
        RawMode {}
    }

    pub(crate) fn enter(&self) -> Result<bool> {
        let mut raw_mode = RAW_MODE.lock();
        let entered = raw_mode.enter()?;
        Ok(entered)
    }

    pub(crate) fn leave(&self) -> Result<bool> {
        let mut raw_mode = RAW_MODE.lock();
        let left = raw_mode.leave()?;
        Ok(left)
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
