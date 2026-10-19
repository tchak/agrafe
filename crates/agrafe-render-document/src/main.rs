//! The `agrafe-render-document` helper: the workers run it in a sandbox
//! with nothing but a workspace directory and an output path. It reads no
//! configuration and writes no logs; what it says on stderr is the
//! compiler's diagnostics, which the job records when it fails. The exit
//! code says whose failure it is: [`REJECTED_EXIT`] for a document that
//! does not compile, 1 for anything the helper itself could not do.

use std::path::PathBuf;
use std::process::ExitCode;

use agrafe_render_document::{CompileError, OpenError, REJECTED_EXIT, Workspace};
use clap::Parser;

/// Compile a typst workspace into a PDF. Exits 2, with the diagnostics on
/// stderr, when the document does not compile; 1 when the helper could
/// not read the workspace or write the output.
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
            match err {
                Failure::Rejected(_) => ExitCode::from(REJECTED_EXIT),
                Failure::Open(_) | Failure::Write(_) => ExitCode::FAILURE,
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum Failure {
    /// The document's: it does not compile.
    #[error(transparent)]
    Rejected(CompileError),
    /// The helper's: the workspace the job wrote cannot be read, or the
    /// PDF cannot be written. The job validated the main path at creation.
    #[error(transparent)]
    Open(OpenError),
    #[error("writing the PDF: {0}")]
    Write(std::io::Error),
}

fn run(args: &Args) -> Result<(), Failure> {
    let workspace = Workspace::open(&args.workspace, &args.main).map_err(Failure::Open)?;
    let pdf = workspace.compile().map_err(Failure::Rejected)?;
    std::fs::write(&args.output, pdf).map_err(Failure::Write)?;
    Ok(())
}
