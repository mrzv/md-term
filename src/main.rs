mod app;
mod graphics;
mod render;

use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::Parser;

/// Graphical Markdown viewer for Kitty, Ghostty, and WezTerm.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Markdown documents to open in tabs; use - for stdin.
    /// With no files, read piped stdin or browse the current directory.
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
    let mut args = Args::parse();
    if args.files.is_empty() && !io::stdin().is_terminal() {
        args.files.push(PathBuf::from("-"));
    }
    if args.export.is_some() && args.files.len() != 1 {
        bail!("--export requires exactly one Markdown document");
    }
    if args
        .files
        .iter()
        .filter(|path| path.as_os_str() == "-")
        .count()
        > 1
    {
        bail!("stdin may only be specified once");
    }
    if args.export.is_none() && !io::stdout().is_terminal() {
        bail!(
            "interactive viewing requires terminal output; use --export PNG for noninteractive rendering"
        );
    }
    let mut sources = args
        .files
        .into_iter()
        .map(|path| {
            let source = if path == Path::new("-") {
                render::Source::stdin(&mut io::stdin().lock())
            } else {
                render::Source::file(&path)
            }?;
            Ok(Arc::new(source))
        })
        .collect::<Result<Vec<_>>>()?;
    let options = render::RenderOptions {
        dark: !args.light,
        ppi: args.ppi,
    };
    if let Some(output) = args.export {
        return render::export(sources.pop().unwrap(), &output, options);
    }
    app::run(sources, options).context("viewer failed")
}

fn main() {
    if let Err(error) = run() {
        eprintln!("mdterm: {error:#}");
        std::process::exit(1);
    }
}
