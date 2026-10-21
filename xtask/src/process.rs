//! Running jj and cargo in the checkout.

use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("running `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("`{0}` failed: {1}")]
    Failed(String, std::process::ExitStatus),
}

fn command(root: &Path, program: &str, args: &[&str]) -> (Command, String) {
    let mut command = Command::new(program);
    command.args(args).current_dir(root);
    (command, format!("{program} {}", args.join(" ")))
}

/// Runs `program args`, its output on the terminal.
pub fn run(root: &Path, program: &str, args: &[&str]) -> Result<(), CommandError> {
    let (mut command, line) = command(root, program, args);
    let status = command
        .status()
        .map_err(|e| CommandError::Spawn(line.clone(), e))?;
    if !status.success() {
        return Err(CommandError::Failed(line, status));
    }
    Ok(())
}

/// Runs `program args` and returns what it printed; its errors go to the
/// terminal.
pub fn output(root: &Path, program: &str, args: &[&str]) -> Result<String, CommandError> {
    let (mut command, line) = command(root, program, args);
    let output = command
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| CommandError::Spawn(line.clone(), e))?;
    if !output.status.success() {
        return Err(CommandError::Failed(line, output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
