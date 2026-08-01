use std::path::PathBuf;

use clap::Parser;

/// A terminal UI for Zed's editor.
#[derive(Parser)]
#[command(name = "ted", version, about)]
struct Args {
    /// Files or directories to open.
    paths: Vec<PathBuf>,

    /// Use Zed's own keybindings instead of vim mode.
    #[arg(long)]
    no_vim: bool,

    /// Paint the theme's editor background instead of letting the terminal's
    /// own background show through.
    #[arg(long)]
    opaque_background: bool,

    /// Use this directory for settings, keymaps and the workspace database
    /// instead of the ones shared with Zed.
    #[arg(long)]
    user_data_dir: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    // Before anything else: `set_custom_data_dir` panics once any path has been
    // resolved, and `ted::frame::run` reaches the settings file almost at once.
    if let Some(dir) = &args.user_data_dir {
        paths::set_custom_data_dir(dir);
    }

    ted::frame::run(ted::frame::Options {
        paths: args.paths,
        vim: !args.no_vim,
        opaque_background: args.opaque_background,
    })
}
