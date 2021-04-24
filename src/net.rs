use crate::prelude::*;
use std::net::SocketAddr;
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
