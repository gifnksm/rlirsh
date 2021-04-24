use crate::prelude::*;
use std::net::{SocketAddr, ToSocketAddrs};

pub(crate) fn socket_addrs(s: &str) -> Result<Vec<SocketAddr>> {
    let addrs = s.to_socket_addrs()?.collect::<Vec<_>>();
    if addrs.is_empty() {
        bail!("failed to lookup address information");
    }
    Ok(addrs)
}

pub(crate) fn local_port_forward_specifier(s: &str) -> Result<(Vec<SocketAddr>, (String, u16))> {
    let parts = s.split(':').collect::<Vec<_>>();
    let (bind_address, port, host, host_port) = match &parts[..] {
        [ba, p, h, hp] => (*ba, *p, *h, *hp),
        [p, h, hp] => ("0.0.0.0", *p, *h, *hp),
        _ => bail!("invalid port forwarding specifier: {:?}", parts),
    };
    let port = port.parse::<u16>().wrap_err("invalid port number")?;
    let host_port = host_port.parse::<u16>().wrap_err("invalid port number")?;

    let local_addrs = (bind_address, port).to_socket_addrs()?.collect::<Vec<_>>();
    if local_addrs.is_empty() {
        bail!("failed to lookup address information");
    }

    Ok((local_addrs, (host.into(), host_port)))
}
