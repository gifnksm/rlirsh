use nix::libc;

nix::ioctl_write_int_bad!(
    /// Make the given terminal the controlling terminal of the calling process.
    ///
    /// The calling process must be a session leader and not have a controlling terminal
    /// already. If the terminal is already the controlling terminal of a different session
    /// group then the ioctl will fail with **EPERM**, unless the caller is root (more
    /// precisely: has the **CAP_SYS_ADMIN** capability) and arg equals 1, in which case the
    /// terminal is stolen and all processes that had it as controlling terminal lose it.
    tiocsctty,
    libc::TIOCSCTTY
);
