use nix::libc;

nix::ioctl_read_bad!(
    /// Get window size.
    tiocgwinsz,
    libc::TIOCGWINSZ,
    libc::winsize
);

nix::ioctl_write_ptr_bad!(
    /// Set window size.
    tiocswinsz,
    libc::TIOCSWINSZ,
    libc::winsize
);
