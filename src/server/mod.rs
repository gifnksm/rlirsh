use crate::{
    net,
    prelude::*,
    protocol::{self, Request},
};
use clap::Clap;
use std::net::{IpAddr, SocketAddr};
use tokio::net::TcpStream;

mod execute;

/// Launch rlirsh server
#[derive(Debug, Clap)]
pub(super) struct Args {
    /// An alternative bind address [default: 0.0.0.0]
    #[clap(long = "bind", default_value = "0.0.0.0")]
    bind: IpAddr,
    /// A port number to bind
    port: u16,
}

pub(super) async fn main(args: Args) -> Result<i32> {
    let addr = SocketAddr::new(args.bind, args.port);
    let listener = net::listen(addr, 1024)?;

    info!("start listening on {:?}", addr);
    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(res) => res,
            Err(err) => {
                warn!(?err, "accept failed");
                continue;
            }
        };

        info!(%peer_addr, "connection accepted");
        let _ = tokio::spawn(
            async move {
                if let Err(err) = serve(stream).await {
                    warn!(?err, "serve failed");
                }
            }
            .instrument(info_span!("serve", %peer_addr)),
        );
    }
}

async fn serve(mut stream: TcpStream) -> Result<()> {
    let request = protocol::recv_message(&mut stream)
        .await
        .wrap_err("failed to receive request")?;
    match request {
        Request::Execute(req) => execute::main(stream, req).await?,
    }
    Ok(())
}
