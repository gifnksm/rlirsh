use crate::{net::SocketAddrs, prelude::*};

pub(crate) fn local_port_forward_specifier(s: &str) -> Result<(SocketAddrs, String)> {
    let parts = s.split(':').collect::<Vec<_>>();
    let (local_addr, remote_addr) = match &parts[..] {
        [ba, p, h, hp] => (format!("{}:{}", ba, p), format!("{}:{}", h, hp)),
        [p, h, hp] => (format!("0.0.0.0:{}", p), format!("{}:{}", h, hp)),
        _ => bail!("invalid port forwarding specifier: {:?}", parts),
    };
    let local_addrs = local_addr.parse()?;
    Ok((local_addrs, remote_addr))
}
