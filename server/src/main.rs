//! Replica server binary: parses the command line and hands over to the library.

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use replica_server::config::{Cli, Command, ServeArgs};
use replica_server::storage::Storage;
use replica_server::{cli, client};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve(ServeArgs::parse_from(["serve"])));

    match command {
        Command::Serve(args) => {
            let storage = Storage::open(&cli.data_dir).context("opening data dir")?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(replica_server::serve(args, storage))
        }
        Command::Sync(args) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(client::run(args)),
        Command::User { cmd } => cli::user(&Storage::open(&cli.data_dir)?, cmd),
        Command::Vault { cmd } => cli::vault(&Storage::open(&cli.data_dir)?, cmd),
        Command::Snapshot { target, keep } => cli::snapshot(&Storage::open(&cli.data_dir)?, target, keep),
        Command::Backup { out, user, vault } => cli::backup(&Storage::open(&cli.data_dir)?, out, user, vault),
        Command::Restore { from, merge } => cli::restore(&cli.data_dir, &from, merge),
        Command::RestoreSnapshot { target, file, yes } => {
            cli::restore_snapshot(&Storage::open(&cli.data_dir)?, &target, &file, yes)
        }
    }
}

