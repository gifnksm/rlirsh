use crate::{
    prelude::*,
    protocol::{self, Request},
};
use clap::Clap;
use std::net::{IpAddr, SocketAddr};
use tokio::net::{TcpSocket, TcpStream};

mod execute;

/// Launch rlirsh server
#[derive(Debug, Clap)]
pub(super) struct Args {
    /// alternative bind address [default: 0.0.0.0]
    #[clap(long = "bind", default_value = "0.0.0.0")]
    bind: IpAddr,
    /// port number to bind
    port: u16,
}

pub(super) async fn main(args: Args) -> Result<()> {
    let addr = SocketAddr::new(args.bind, args.port);
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }?;
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    let listener = socket.listen(1024)?;
    info!("start listening on {:?}", addr);
    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                info!(%peer_addr, "connected");
                tokio::spawn(
                    async move {
                        if let Err(err) = serve(stream).await {
                            warn!(?err, "serve failed");
                        }
                    }
                    .instrument(info_span!("serve", %peer_addr)),
                );
            }
            Err(err) => {
                warn!(?err, "accept failed");
                continue;
            }
        };
    }
}

async fn serve(mut stream: TcpStream) -> Result<()> {
    let request = protocol::recv_message(&mut stream)
        .await
        .wrap_err("failed to receive request")?;
    match request {
        Request::Execute(req) => execute::serve(stream, req).await?,
    }
    Ok(())
}
