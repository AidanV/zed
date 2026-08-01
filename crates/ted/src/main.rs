use std::path::PathBuf;

use clap::Parser;

/// A terminal UI for Zed's editor.
#[derive(Parser)]
#[command(name = "ted", version, about)]
struct Args {
    /// File to open.
    path: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    ted::frame::run(ted::frame::Options { path: args.path })
}
