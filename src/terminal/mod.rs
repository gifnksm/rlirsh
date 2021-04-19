use crate::{ioctl, prelude::*};
use nix::unistd;
use std::{mem, os::unix::prelude::AsRawFd};

pub(crate) mod raw_mode;

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
