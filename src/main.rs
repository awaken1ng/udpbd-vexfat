use std::path::PathBuf;

use clap::Parser;
use server::Server;

mod protocol;
mod server;
mod vexfat;
mod utils;

#[derive(Parser, Debug)]
#[command(version, arg_required_else_help = true)]
pub struct Args {
    /// Path to directory to map as DVD directory in vexFAT.
    pub path: PathBuf,

    /// OPL prefix.
    #[arg(short, long)]
    pub prefix: Option<String>,
}

fn main() {
    let args = Args::parse();

    Server::new(&args).unwrap().run();
}
