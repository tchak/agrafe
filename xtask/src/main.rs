//! The workspace's own tasks, run as `cargo xtask <task>` from anywhere in
//! the checkout (the alias is in `.cargo/config.toml`).
//!
//! `main` takes commits through pull requests only, so a release is two
//! tasks, either side of a merge:
//!
//! - `release <version>` prepares one: it bumps the workspace's version
//!   and its internal requirements, gives the changelog's Unreleased
//!   section the version and today's date, commits `Release <version>` on
//!   top of main under the `release/v<version>` bookmark and, once
//!   confirmed, pushes the bookmark and opens its pull request with `gh`.
//!   A pre-release leaves the changelog alone: Unreleased is its notes,
//!   and stays Unreleased until the release, which gets the section.
//! - `tag <version>`, once that pull request is merged, tags the commit
//!   GitHub merged it as `v<version>`, after checking that it is on main
//!   and carries the version and its notes, and, once confirmed, pushes
//!   the tag alone. The tag starts `.github/workflows/release.yml`.
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
    /// Prepare a release: bump, date the changelog, commit on a
    /// `release/v<version>` bookmark, then push it and open its pull
    /// request once asked. Run it from an empty working copy on top of
    /// main, as `jj new main` leaves it.
    Release {
        /// The version to release, such as 0.2.0 or 0.2.0-rc.1.
        version: String,
    },
    /// Tag a merged release pull request's commit on main `v<version>` and
    /// push the tag once asked, which starts the release workflow.
    Tag {
        /// The version released, such as 0.2.0 or 0.2.0-rc.1.
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
    #[error("{0} exists, here or on origin: a release of it is under way")]
    BookmarkExists(String),
    #[error(
        "the workspace is at {0} already and a pre-release dates no changelog: \
         there is nothing to commit"
    )]
    NothingToRelease(Version),
    #[error("no merged pull request from {0}: merge it first")]
    NotMerged(String),
    #[error("{0}, the release pull request's merge, is not on main@origin")]
    MergeNotOnMain(String),
    #[error("{repository} is not a GitHub repository")]
    NotGitHub { repository: String },
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
        Task::Tag { version } => parse(&version).and_then(|v| tag(root, &v)),
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

/// The bookmark a release of `version` is prepared on, and its pull
/// request opened from.
fn bookmark(version: &Version) -> String {
    format!("release/v{version}")
}

/// `gh`'s `--repo` for the repository URL in Cargo.toml.
fn gh_repo(repository: &str) -> Result<&str, Error> {
    repository
        .strip_prefix("https://")
        .filter(|rest| rest.starts_with("github.com/"))
        .ok_or_else(|| Error::NotGitHub {
            repository: repository.into(),
        })
}

/// Asks `question` on the terminal; only `y` is a yes.
fn confirm(question: &str) -> Result<bool, Error> {
    print!("{question} [y/N] ");
    std::io::stdout().flush().map_err(Error::Prompt)?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(Error::Prompt)?;
    Ok(matches!(answer.trim(), "y" | "Y"))
}

/// The versions tagged here or on any remote, refusing `version` if it is
/// among them or not after the last one, which it must follow.
fn released(root: &Path, version: &Version) -> Result<Vec<Version>, Error> {
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
    if let Some(previous) = released.last()
        && previous >= version
    {
        return Err(Error::NotAfter {
            version: version.clone(),
            previous: previous.clone(),
        });
    }
    Ok(released)
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
    let bookmark = bookmark(version);
    let manifest_path = root.join("Cargo.toml");
    let changelog_path = root.join("CHANGELOG.md");
    let mut manifest = read(&manifest_path)?;
    let current = manifest::version(&manifest)?;
    let repository = manifest::repository(&manifest)?;
    let gh_repo = gh_repo(&repository)?;
    if *version == current && !version.pre.is_empty() {
        return Err(Error::NothingToRelease(version.clone()));
    }

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
    let pattern = format!("exact:{bookmark:?}");
    let taken = process::output(
        root,
        "jj",
        &[
            "log",
            "--no-graph",
            "-r",
            &format!("bookmarks({pattern}) | remote_bookmarks({pattern})"),
            "-T",
            "commit_id",
        ],
    )?;
    if !taken.trim().is_empty() {
        return Err(Error::BookmarkExists(bookmark));
    }

    // The last release, which the changelog's links compare against.
    let released = released(root, version)?;
    let last_release = version::last_release(&released);

    // Where `jj op restore` takes the repository back to if the release is
    // not pushed.
    let start = process::output(
        root,
        "jj",
        &["op", "log", "--no-graph", "-n", "1", "-T", "id.short(12)"],
    )?;
    let start = start.trim();

    // Before anything is written: a changelog it refuses leaves the tree
    // as it was.
    let mut changelog = read(&changelog_path)?;
    changelog::unreleased(&changelog)?;
    if version.pre.is_empty() {
        let today = jiff::Zoned::now().date();
        changelog = changelog::release(
            &changelog,
            version,
            &today.to_string(),
            &repository,
            last_release,
        )?;
    }
    let body = format!(
        "{}\n\n---\n\nOnce this is merged, `cargo xtask tag {version}` tags the \
         merged commit `v{version}`, which starts the release workflow.\n",
        changelog::notes(&changelog, version)?
    );

    if *version != current {
        manifest = manifest::bump(&manifest, &current, version)?;
        write(&manifest_path, &manifest)?;
        process::run(root, "cargo", &["update", "--workspace", "--quiet"])?;
    }
    if version.pre.is_empty() {
        write(&changelog_path, &changelog)?;
    }

    let title = format!("Release {version}");
    process::run(root, "jj", &["commit", "-m", &title])?;
    process::run(root, "jj", &["bookmark", "set", &bookmark, "-r", "@-"])?;
    process::run(root, "jj", &["show", "--stat", "@-"])?;

    if confirm(&format!("Push {bookmark} and open its pull request?"))? {
        process::run(
            root,
            "jj",
            &["git", "push", "--remote", "origin", "--bookmark", &bookmark],
        )?;
        process::run(
            root,
            "gh",
            &[
                "pr", "create", "--repo", gh_repo, "--base", "main", "--head", &bookmark,
                "--title", &title, "--body", &body,
            ],
        )?;
        println!("Once it is merged: cargo xtask tag {version}");
    } else {
        println!(
            "Not pushed. Later: jj git push --remote origin --bookmark {bookmark}, \
             then gh pr create --base main --head {bookmark}"
        );
        println!("Or drop it: jj op restore {start}");
    }
    Ok(())
}

fn tag(root: &Path, version: &Version) -> Result<(), Error> {
    let tag = format!("v{version}");
    let bookmark = bookmark(version);
    let repository = manifest::repository(&read(&root.join("Cargo.toml"))?)?;
    let gh_repo = gh_repo(&repository)?;

    process::run(root, "jj", &["git", "fetch", "--remote", "origin"])?;
    released(root, version)?;

    // GitHub's own answer for what the pull request became on main, whatever
    // the merge method: the squashed commit, the last one rebased, or the
    // merge commit.
    let merged = process::output(
        root,
        "gh",
        &[
            "pr",
            "list",
            "--repo",
            gh_repo,
            "--head",
            &bookmark,
            "--base",
            "main",
            "--state",
            "merged",
            "--json",
            "mergeCommit",
            "--jq",
            ".[0].mergeCommit.oid // empty",
        ],
    )?;
    let commit = merged.trim();
    if commit.is_empty() {
        return Err(Error::NotMerged(bookmark));
    }
    let on_main = process::output(
        root,
        "jj",
        &[
            "log",
            "--no-graph",
            "-r",
            &format!("{commit} & ::main@origin"),
            "-T",
            "commit_id",
        ],
    )?;
    if on_main.trim().is_empty() {
        return Err(Error::MergeNotOnMain(commit.into()));
    }

    // What the workflow's first job checks, checked before the tag is
    // pushed rather than after.
    let manifest = process::output(root, "jj", &["file", "show", "-r", commit, "Cargo.toml"])?;
    let at = manifest::version(&manifest)?;
    if at != *version {
        return Err(Error::NotAtVersion {
            manifest: at,
            version: version.clone(),
        });
    }
    let changelog = process::output(root, "jj", &["file", "show", "-r", commit, "CHANGELOG.md"])?;
    changelog::notes(&changelog, version)?;

    process::run(root, "jj", &["tag", "set", &tag, "-r", commit])?;
    process::run(root, "jj", &["log", "--no-graph", "-r", commit])?;

    if confirm(&format!(
        "Push {tag} to origin? It starts the release workflow, which publishes the crates."
    ))? {
        process::run(
            root,
            "jj",
            &["git", "push", "--remote", "origin", "--tag", &tag],
        )?;
        println!("Pushed. The release workflow: {repository}/actions/workflows/release.yml");
    } else {
        println!("Not pushed. Later: jj git push --remote origin --tag {tag}");
        println!("Or drop it: jj tag delete {tag}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_takes_the_repository_without_its_scheme() {
        assert_eq!(
            gh_repo("https://github.com/tchak/agrafe").unwrap(),
            "github.com/tchak/agrafe"
        );
        assert!(gh_repo("https://gitlab.com/tchak/agrafe").is_err());
        assert!(gh_repo("git@github.com:tchak/agrafe").is_err());
    }

    #[test]
    fn a_release_is_prepared_on_its_own_bookmark() {
        let version = Version::parse("0.2.0-rc.1").unwrap();
        assert_eq!(bookmark(&version), "release/v0.2.0-rc.1");
    }
}
