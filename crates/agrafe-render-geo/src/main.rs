//! The `agrafe-render-geo` helper: the workers run it in a sandbox with
//! the GeoJSON file and the spec the server wrote. It reads no
//! configuration, writes no logs and never touches the network: the
//! background is the server's business, from the extent written here.

use std::path::PathBuf;
use std::process::ExitCode;

use agrafe_types::geojson::Spec;
use clap::Parser;

/// Draw a GeoJSON file as SVG paths over the extent it fits in. Writes
/// `svg` and `extent` only when the file holds geometry; exits zero either
/// way. A file that cannot be drawn, with a coordinate off the globe,
/// exits non-zero with the reason on stderr.
#[derive(Parser)]
#[command(name = "agrafe-render-geo")]
struct Args {
    /// The GeoJSON file.
    #[arg(long)]
    input: PathBuf,
    /// The JSON spec: pixel box, margins, style, background layers.
    #[arg(long)]
    spec: PathBuf,
    /// Where the SVG goes.
    #[arg(long)]
    svg: PathBuf,
    /// Where the extent goes, as JSON.
    #[arg(long)]
    extent: PathBuf,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("reading the spec: {0}")]
    Spec(serde_json::Error),
    #[error("{0}")]
    Render(#[from] agrafe_render_geo::RenderError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("writing the extent: {0}")]
    Extent(serde_json::Error),
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("agrafe-render-geo: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Error> {
    let spec: Spec = serde_json::from_slice(&std::fs::read(&args.spec)?).map_err(Error::Spec)?;
    let bytes = std::fs::read(&args.input)?;
    if let Some(rendered) = agrafe_render_geo::render(&bytes, &spec)? {
        std::fs::write(&args.svg, rendered.svg)?;
        let extent = serde_json::to_vec(&rendered.extent).map_err(Error::Extent)?;
        std::fs::write(&args.extent, extent)?;
    }
    Ok(())
}
