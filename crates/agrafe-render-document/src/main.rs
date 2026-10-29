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
/// stderr, when the document does not compile or compiles past
/// `--max-output`; 1 when the helper could not read the workspace or write
/// the output.
#[derive(Parser)]
#[command(name = "agrafe-render-document")]
struct Args {
    /// The directory holding the template and every file it uses.
    #[arg(long)]
    workspace: PathBuf,
    /// The entry file, relative to the workspace; it may start with `-`.
    #[arg(long, allow_hyphen_values = true)]
    main: String,
    #[arg(long)]
    output: PathBuf,
    /// The largest PDF written, the sandbox's file size limit: a larger
    /// one is the document's verdict, not a write the kernel kills.
    #[arg(long)]
    max_output: u64,
}

fn main() -> ExitCode {
    // clap exits 2 on a usage error, which is the document's verdict here:
    // a command line the helper does not take is the helper's failure.
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            match err {
                Failure::Rejected(_) | Failure::TooLarge { .. } => ExitCode::from(REJECTED_EXIT),
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
    /// The document's: it compiles to more than the helper may write.
    #[error("the PDF is {len} bytes, more than the {max} media.sandbox.max_output_size allows")]
    TooLarge { len: u64, max: u64 },
}

fn run(args: &Args) -> Result<(), Failure> {
    let workspace = Workspace::open(&args.workspace, &args.main).map_err(Failure::Open)?;
    let pdf = workspace.compile().map_err(Failure::Rejected)?;
    let len = pdf.len() as u64;
    if len > args.max_output {
        return Err(Failure::TooLarge {
            len,
            max: args.max_output,
        });
    }
    std::fs::write(&args.output, pdf).map_err(Failure::Write)?;
    Ok(())
}
