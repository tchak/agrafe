//! The `agrafe-extract` helper: the workers run it in a sandbox with
//! nothing but its two files. It reads no configuration and writes no logs.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Pull the embedded image out of a document. Writes `output` only when an
/// image was found; exits zero either way.
#[derive(Parser)]
#[command(name = "agrafe-extract")]
struct Args {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    content_type: String,
    #[arg(long)]
    min_coverage: f32,
    #[arg(long)]
    max_text_chars: u32,
    /// Largest decoded buffer, in bytes.
    #[arg(long)]
    max_output: usize,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("agrafe-extract: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> std::io::Result<()> {
    let bytes = std::fs::read(&args.input)?;
    let thresholds = agrafe_extract::Thresholds {
        min_coverage: args.min_coverage,
        max_text_chars: args.max_text_chars,
    };
    if let Some(found) =
        agrafe_extract::extract(&args.content_type, &bytes, &thresholds, args.max_output)
    {
        use std::io::Write as _;
        let mut output = std::fs::File::create(&args.output)?;
        output.write_all(&found.header)?;
        output.write_all(&found.bytes)?;
    }
    Ok(())
}
