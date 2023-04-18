use std::path::PathBuf;

use clap::Parser;
use server::Server;

mod protocol;
mod server;
mod vexfat;

#[derive(Parser, Debug)]
#[command(version, arg_required_else_help = true)]
pub struct Args {
    /// File to map into virtual file system
    pub file: PathBuf,

    /// OPL prefix
    #[arg(long)]
    pub prefix: Option<String>,
}

fn main() {
    let args = Args::parse();

    Server::new(&args).unwrap().run();
}
