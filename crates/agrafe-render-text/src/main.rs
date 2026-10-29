//! The `agrafe-render-text` helper: the workers run it in a sandbox with
//! nothing but its two files. It reads no configuration and writes no logs.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Rasterise the first lines of a text file. Writes `output`, a PGM, only
/// when the input looks like text; exits zero either way.
#[derive(Parser)]
#[command(name = "agrafe-render-text")]
struct Args {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    width: u32,
    #[arg(long)]
    height: u32,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("agrafe-render-text: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> std::io::Result<()> {
    use std::io::Read as _;
    // The server hands no more; whoever else runs this gets the same page.
    let mut bytes = Vec::new();
    std::fs::File::open(&args.input)?
        .take(agrafe_types::text::SAMPLE_LEN)
        .read_to_end(&mut bytes)?;
    if let Some(pgm) = agrafe_render_text::render(&bytes, args.width, args.height) {
        std::fs::write(&args.output, pgm)?;
    }
    Ok(())
}
