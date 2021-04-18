use std::io;

mod command_ext;
mod ioctl;
mod pty_master;

pub type Error = io::Error;
pub type Result<T> = io::Result<T>;
pub use command_ext::*;
pub use pty_master::*;

trait IntoStd {
    type Output;
    fn into_std(self) -> Self::Output;
}

impl IntoStd for nix::Error {
    type Output = Error;
    fn into_std(self) -> Self::Output {
        self.as_errno().unwrap().into()
    }
}

impl<T> IntoStd for nix::Result<T> {
    type Output = Result<T>;
    fn into_std(self) -> Self::Output {
        self.map_err(IntoStd::into_std)
    }
}
