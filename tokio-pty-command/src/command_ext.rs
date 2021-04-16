use crate::{ioctl, IntoStd, PtyMaster, Result};
use nix::{libc, unistd};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};
use tokio::process::{Child, Command};

pub trait CommandExt {
    fn spawn_with_pty(&mut self, pty_master: &PtyMaster) -> Result<Child>;
}

impl CommandExt for Command {
    fn spawn_with_pty(&mut self, pty_master: &PtyMaster) -> Result<Child> {
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(pty_master.slave_name())?;

        self.stdin(slave.try_clone()?);
        self.stdout(slave.try_clone()?);
        self.stderr(slave.try_clone()?);

        unsafe {
            self.pre_exec(move || {
                let _pid = unistd::setsid().into_std()?;
                ioctl::tiocsctty(libc::STDIN_FILENO, 1).into_std()?;
                Ok(())
            });
        }

        self.spawn()
    }
}
