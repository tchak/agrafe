//! The workspace's own tasks, run as `cargo xtask <task>` from anywhere in
//! the checkout (the alias is in `.cargo/config.toml`).
//!
//! - `release <version>` cuts a release of every crate at one version: it
//!   bumps the workspace's version and its internal requirements, gives the
//!   changelog's Unreleased section the version and today's date, commits
//!   `Release <version>` on top of main, tags it `v<version>` and, once
//!   confirmed, pushes main and the tag. The tag starts
//!   `.github/workflows/release.yml`. A pre-release leaves the changelog
//!   alone: Unreleased is its notes, and stays Unreleased until the
//!   release, which gets the section.
//! - `notes <version>` prints a version's release notes, what the
//!   workflow puts on the GitHub release, after checking that the
//!   workspace is at that version.

mod changelog;
mod manifest;
mod process;
mod version;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use semver::Version;

use crate::changelog::ChangelogError;
use crate::manifest::ManifestError;
use crate::process::CommandError;

#[derive(Parser)]
#[command(name = "cargo xtask", about = "The workspace's own tasks")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Cut a release: bump, date the changelog, commit, tag, and push once
    /// asked. Run it from an empty working copy on top of main, as `jj new
    /// main` leaves it.
    Release {
        /// The version to release, such as 0.2.0 or 0.2.0-rc.1.
        version: String,
    },
    /// Print a version's release notes: its changelog section, or
    /// Unreleased for a pre-release.
    Notes {
        /// The version released, such as 0.2.0 or 0.2.0-rc.1.
        version: String,
    },
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0} is not a version: {1}")]
    Version(String, semver::Error),
    #[error("{0}: a release version has no build metadata")]
    BuildMetadata(Version),
    #[error("reading {}: {err}", path.display())]
    Read { path: PathBuf, err: std::io::Error },
    #[error("writing {}: {err}", path.display())]
    Write { path: PathBuf, err: std::io::Error },
    #[error("Cargo.toml: {0}")]
    Manifest(#[from] ManifestError),
    #[error("CHANGELOG.md: {0}")]
    Changelog(#[from] ChangelogError),
    #[error("{0}")]
    Command(#[from] CommandError),
    #[error("the workspace is at {manifest}, not {version}")]
    NotAtVersion { manifest: Version, version: Version },
    #[error("the working copy has changes; start from `jj new main`")]
    Dirty,
    #[error("the working copy's parent must be main, and main what origin has")]
    NotOnMain,
    #[error("v{0} exists")]
    TagExists(Version),
    #[error("{version} is not after v{previous}")]
    NotAfter { version: Version, previous: Version },
    #[error("reading the answer: {0}")]
    Prompt(std::io::Error),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is a directory of the workspace");
    let result = match cli.task {
        Task::Release { version } => parse(&version).and_then(|v| release(root, &v)),
        Task::Notes { version } => parse(&version).and_then(|v| notes(root, &v)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

fn parse(version: &str) -> Result<Version, Error> {
    let parsed = Version::parse(version).map_err(|e| Error::Version(version.into(), e))?;
    if !parsed.build.is_empty() {
        return Err(Error::BuildMetadata(parsed));
    }
    Ok(parsed)
}

fn read(path: &Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|err| Error::Read {
        path: path.into(),
        err,
    })
}

fn write(path: &Path, contents: &str) -> Result<(), Error> {
    std::fs::write(path, contents).map_err(|err| Error::Write {
        path: path.into(),
        err,
    })
}

fn notes(root: &Path, version: &Version) -> Result<(), Error> {
    let manifest = manifest::version(&read(&root.join("Cargo.toml"))?)?;
    if manifest != *version {
        return Err(Error::NotAtVersion {
            manifest,
            version: version.clone(),
        });
    }
    let changelog = read(&root.join("CHANGELOG.md"))?;
    println!("{}", changelog::notes(&changelog, version)?);
    Ok(())
}

fn release(root: &Path, version: &Version) -> Result<(), Error> {
    let tag = format!("v{version}");
    let manifest_path = root.join("Cargo.toml");
    let changelog_path = root.join("CHANGELOG.md");
    let mut manifest = read(&manifest_path)?;
    let current = manifest::version(&manifest)?;
    let repository = manifest::repository(&manifest)?;

    process::run(root, "jj", &["git", "fetch", "--remote", "origin"])?;
    if !process::output(root, "jj", &["diff", "--summary"])?
        .trim()
        .is_empty()
    {
        return Err(Error::Dirty);
    }
    let on_main = process::output(
        root,
        "jj",
        &[
            "log",
            "--no-graph",
            "-r",
            "@- & main & main@origin",
            "-T",
            "commit_id",
        ],
    )?;
    if on_main.trim().is_empty() {
        return Err(Error::NotOnMain);
    }

    let tags = process::output(
        root,
        "jj",
        &[
            "tag",
            "list",
            "--all-remotes",
            "-T",
            "name ++ \"\\n\"",
            "glob:v*",
        ],
    )?;
    let released = version::tagged(&tags);
    if released.contains(version) {
        return Err(Error::TagExists(version.clone()));
    }
    // The last tag, which this one must follow, and the last release, which
    // the changelog's links compare against.
    if let Some(previous) = released.last()
        && previous >= version
    {
        return Err(Error::NotAfter {
            version: version.clone(),
            previous: previous.clone(),
        });
    }
    let last_release = version::last_release(&released);

    // Where `jj op restore` takes the repository back to if the release is
    // not pushed.
    let start = process::output(
        root,
        "jj",
        &["op", "log", "--no-graph", "-n", "1", "-T", "id.short(12)"],
    )?;
    let start = start.trim();

    let mut changelog = read(&changelog_path)?;
    changelog::unreleased(&changelog)?;

    if *version != current {
        manifest = manifest::bump(&manifest, &current, version)?;
        write(&manifest_path, &manifest)?;
        process::run(root, "cargo", &["update", "--workspace", "--quiet"])?;
    }
    if version.pre.is_empty() {
        let today = jiff::Zoned::now().date();
        changelog = changelog::release(
            &changelog,
            version,
            &today.to_string(),
            &repository,
            last_release,
        )?;
        write(&changelog_path, &changelog)?;
    }

    process::run(root, "jj", &["commit", "-m", &format!("Release {version}")])?;
    process::run(root, "jj", &["tag", "set", &tag, "-r", "@-"])?;
    process::run(root, "jj", &["bookmark", "set", "main", "-r", "@-"])?;
    process::run(root, "jj", &["show", "--stat", "@-"])?;

    print!("Push main and {tag} to origin? [y/N] ");
    std::io::stdout().flush().map_err(Error::Prompt)?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(Error::Prompt)?;
    if matches!(answer.trim(), "y" | "Y") {
        process::run(
            root,
            "jj",
            &[
                "git",
                "push",
                "--remote",
                "origin",
                "--bookmark",
                "main",
                "--tag",
                &tag,
            ],
        )?;
        println!("Pushed. The release workflow: {repository}/actions/workflows/release.yml");
    } else {
        println!("Not pushed. Later: jj git push --remote origin --bookmark main --tag {tag}");
        println!("Or drop it: jj op restore {start}");
    }
    Ok(())
}
