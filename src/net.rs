use crate::prelude::*;
use std::{
    fmt::{self, Display},
    net::{SocketAddr, ToSocketAddrs},
    str::FromStr,
};
use tokio::{
    io,
    net::{TcpListener, TcpSocket, TcpStream},
};

pub(crate) fn listen(addr: SocketAddr, backlog: u32) -> Result<TcpListener> {
    let socket = create_socket(&addr)?;
    socket
        .set_reuseaddr(true)
        .wrap_err("failed to set socket option")?;
    socket
        .bind(addr)
        .wrap_err_with(|| format!("failed to bind address `{}`", addr))?;
    let listener = socket
        .listen(backlog)
        .wrap_err_with(|| format!("failed to listen the socket on address `{}`", addr))?;
    Ok(listener)
}

pub(crate) async fn connect(addr: SocketAddr) -> Result<TcpStream> {
    let socket = create_socket(&addr)?;
    let stream = socket
        .connect(addr)
        .await
        .wrap_err_with(|| format!("failed to connect to `{}`", addr))?;
    Ok(stream)
}

fn create_socket(addr: &SocketAddr) -> io::Result<TcpSocket> {
    if addr.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SocketAddrs {
    original_addr: String,
    addrs: Vec<SocketAddr>,
}

impl FromStr for SocketAddrs {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s, s)
    }
}

impl Display for SocketAddrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.original_addr, f)
    }
}

impl SocketAddrs {
    fn new(original_addr: impl Into<String>, addrs: impl ToSocketAddrs) -> Result<Self> {
        let original_addr = original_addr.into();
        let addrs = addrs
            .to_socket_addrs()
            .wrap_err_with(|| {
                format!(
                    "failed to lookup address information for `{}`",
                    &original_addr
                )
            })?
            .collect::<Vec<_>>();
        ensure!(
            !addrs.is_empty(),
            "failed to lookup address information for `{}`",
            original_addr
        );
        Ok(Self {
            original_addr,
            addrs,
        })
    }

    pub(crate) fn listen(&self, backlog: u32) -> Result<TcpListener> {
        for addr in self.addrs.iter().copied() {
            debug!("listening on {}", addr);
            match listen(addr, backlog) {
                Ok(listener) => return Ok(listener),
                Err(err) => {
                    debug!(?addr, ?err, "failed to listen on the addr");
                }
            }
        }
        bail!("failed to listen on the address `{}`", self.original_addr);
    }

    pub(crate) async fn connect(&self) -> Result<TcpStream> {
        for addr in self.addrs.iter().copied() {
            debug!("connecting to {}", addr);
            match connect(addr).await {
                Ok(stream) => return Ok(stream),
                Err(err) => debug!(?addr, ?err, "failed to connect to the addr"),
            }
        }
        bail!("failed to connect to the address `{}`", self.original_addr);
    }
}
