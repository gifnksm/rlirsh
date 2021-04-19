use crate::{ioctl, prelude::*};
use nix::unistd;
use parking_lot::Mutex;
use std::{mem, os::unix::prelude::AsRawFd, panic};

pub(crate) fn get_window_size(fd: &impl AsRawFd) -> Result<(u16, u16)> {
    let fd = fd.as_raw_fd();
    let winsz = unsafe {
        let mut winsz = mem::zeroed();
        ioctl::tiocgwinsz(fd, &mut winsz)?;
        winsz
    };
    Ok((winsz.ws_col, winsz.ws_row))
}

pub(crate) fn set_window_size(fd: &impl AsRawFd, w: u16, h: u16) -> Result<()> {
    let fd = fd.as_raw_fd();
    let winsz = libc::winsize {
        ws_col: w,
        ws_row: h,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        ioctl::tiocswinsz(fd, &winsz)?;
    };
    Ok(())
}

pub(crate) fn has_tty() -> bool {
    unistd::isatty(libc::STDIN_FILENO).unwrap_or(false)
}

static RAW_MODE: Mutex<imp::RawMode> = Mutex::const_new(
    parking_lot::lock_api::RawMutex::INIT,
    imp::RawMode::new(libc::STDIN_FILENO),
);

pub(crate) fn enter_raw_mode() -> Result<bool> {
    let mut raw_mode = RAW_MODE.lock();
    let entered = raw_mode.enter()?;
    Ok(entered)
}

pub(crate) fn enter_raw_mode_scoped() -> Result<RawModeGuard> {
    trace!("enter raw mode");
    enter_raw_mode()?;
    Ok(RawModeGuard {})
}

pub(crate) fn leave_raw_mode() -> Result<bool> {
    let mut raw_mode = RAW_MODE.lock();
    let left = raw_mode.leave()?;
    trace!("leave raw mode");
    Ok(left)
}

pub(crate) fn leave_raw_mode_on_panic() {
    let saved_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let mut raw_mode = RAW_MODE.lock();
        let left = raw_mode.leave().expect("failed to restore terminal mode");
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
        leave_raw_mode().expect("failed to restore terminal mode");
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
