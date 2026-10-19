//! The `agrafe-render-document` helper: the workers run it in a sandbox
//! with nothing but a workspace directory and an output path. It reads no
//! configuration and writes no logs; what it says on stderr is the
//! compiler's diagnostics, which the job records when it fails.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Compile a typst workspace into a PDF. Exits non-zero, with the
/// diagnostics on stderr, when the document does not compile.
#[derive(Parser)]
#[command(name = "agrafe-render-document")]
struct Args {
    /// The directory holding the template and every file it uses.
    #[arg(long)]
    workspace: PathBuf,
    /// The entry file, relative to the workspace.
    #[arg(long)]
    main: String,
    #[arg(long)]
    output: PathBuf,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = agrafe_render_document::Workspace::open(&args.workspace, &args.main)?;
    let pdf = workspace.compile()?;
    std::fs::write(&args.output, pdf)?;
    Ok(())
}
