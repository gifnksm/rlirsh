use crate::prelude::*;
use clap::Clap;
use std::fmt::Debug;

mod execute;
mod prelude;
mod protocol;
mod server;
mod sink;
mod source;
mod stdin;

/// Rootless insecure remote shell
#[derive(Debug, Clap)]
struct Args {
    #[clap(subcommand)]
    command: SubCommand,
}

#[derive(Debug, Clap)]
enum SubCommand {
    Server(server::Args),
    Execute(execute::Args),
}

fn init_tracing() {
    use tracing::Level;
    use tracing_error::ErrorLayer;
    use tracing_subscriber::{fmt, EnvFilter};
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .with(ErrorLayer::default())
        .init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();

    let args = Args::parse();
    match args.command {
        SubCommand::Server(server_args) => server::main(server_args).await?,
        SubCommand::Execute(execute_args) => execute::main(execute_args).await?,
    }

    Ok(())
}
