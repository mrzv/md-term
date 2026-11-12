mod app;
mod graphics;
mod render;

use std::{io::IsTerminal, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

/// Graphical Markdown viewer for Kitty, Ghostty, and WezTerm.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Markdown documents to open in tabs; omit to browse the current directory.
    files: Vec<PathBuf>,
    /// Use light document colors instead of dark colors.
    #[arg(long)]
    light: bool,
    /// Raster resolution (96 PPI = 100%); defaults to 250% zoom.
    #[arg(long, default_value_t = 240, value_parser = clap::value_parser!(u32).range(36..=288))]
    ppi: u32,
    /// Render the full continuous document to a new PNG without terminal mode.
    #[arg(long, value_name = "PNG")]
    export: Option<PathBuf>,
}

fn run() -> Result<()> {
    let args = Args::parse();
    let options = render::RenderOptions {
        dark: !args.light,
        ppi: args.ppi,
    };
    if let Some(output) = args.export {
        if args.files.len() != 1 {
            bail!("--export requires exactly one Markdown document");
        }
        return render::export(&args.files[0], &output, options);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "interactive viewing requires a terminal; use --export PNG for noninteractive rendering"
        );
    }
    app::run(args.files, options).context("viewer failed")
}

fn main() {
    if let Err(error) = run() {
        eprintln!("mdterm: {error:#}");
        std::process::exit(1);
    }
}
