//! Running decoders and renderers as subprocesses with resource limits.
//!
//! They are the most attackable code in the service, so they never run in
//! the server process: a crash, a decoder bomb or an infinite loop costs one
//! job attempt, not the process. The limits are `setrlimit` on the child and
//! a wall-clock timeout in the parent; on macOS the address space limit is
//! not enforced by the kernel, so only Linux gets the memory bound.

use std::ffi::OsStr;
use std::process::Stdio;
use std::time::Duration;

use bytesize::ByteSize;
use serde::Deserialize;
use tokio::process::Command;

/// Limits on a subprocess that decodes or renders a user file. Deserialises
/// from the `[media.sandbox]` table of the server configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    /// Wall-clock limit; the process is killed past it.
    #[serde(with = "humantime_serde", default = "default_timeout")]
    pub timeout: Duration,
    /// Address space limit (`RLIMIT_AS`).
    #[serde(default = "default_memory")]
    pub memory: ByteSize,
    /// Largest file the process may write (`RLIMIT_FSIZE`).
    #[serde(default = "default_max_output_size")]
    pub max_output_size: ByteSize,
}

fn default_timeout() -> Duration {
    Duration::from_secs(60)
}
fn default_memory() -> ByteSize {
    ByteSize::gib(1)
}
fn default_max_output_size() -> ByteSize {
    ByteSize::mib(256)
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: default_timeout(),
            memory: default_memory(),
            max_output_size: default_max_output_size(),
        }
    }
}

/// Bytes of stderr kept, from the end: the error comes after the warnings.
const STDERR_KEEP: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("starting `{program}`: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("waiting for `{program}`: {source}")]
    Wait {
        program: String,
        source: std::io::Error,
    },
}

/// How a sandboxed process ended.
#[derive(Debug)]
pub enum Outcome {
    Success,
    /// A non-zero exit or a signal, with what the process said on stderr.
    Failed {
        status: String,
        stderr: String,
    },
    /// Killed at the wall-clock limit.
    TimedOut,
}

/// Variables the child inherits. A coverage build adds where the
/// instrumented helpers write their profiles, which would otherwise land
/// in the working directory and out of the report.
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    #[cfg(coverage)]
    "LLVM_PROFILE_FILE",
];

/// A command with a clean environment: the child sees `PATH` and `HOME` and
/// nothing else, in particular no secrets from this process.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    for &name in INHERITED {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

/// Run `command` to completion under `limits`.
pub async fn run(mut command: Command, limits: &Limits) -> Result<Outcome, Error> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_limits(&mut command, limits);
    let child = command.spawn().map_err(|source| Error::Spawn {
        program: program.clone(),
        source,
    })?;
    match tokio::time::timeout(limits.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(Outcome::Success),
        Ok(Ok(output)) => Ok(Outcome::Failed {
            status: output.status.to_string(),
            stderr: tail(&output.stderr),
        }),
        Ok(Err(source)) => Err(Error::Wait { program, source }),
        // Dropping the child on timeout kills it.
        Err(_) => Ok(Outcome::TimedOut),
    }
}

#[cfg(unix)]
fn apply_limits(command: &mut Command, limits: &Limits) {
    let memory = limits.memory.as_u64();
    let cpu = limits.timeout.as_secs().saturating_add(1);
    let output = limits.max_output_size.as_u64();
    let limit = |value: u64| libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: `setrlimit` is async-signal-safe and touches nothing shared
    // with the parent. A limit the hard limit forbids is left as inherited,
    // which is stricter.
    unsafe {
        command.pre_exec(move || {
            libc::setrlimit(libc::RLIMIT_AS, &limit(memory));
            libc::setrlimit(libc::RLIMIT_DATA, &limit(memory));
            libc::setrlimit(libc::RLIMIT_CPU, &limit(cpu));
            libc::setrlimit(libc::RLIMIT_FSIZE, &limit(output));
            libc::setrlimit(libc::RLIMIT_CORE, &limit(0));
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn apply_limits(_command: &mut Command, _limits: &Limits) {}

fn tail(stderr: &[u8]) -> String {
    let start = stderr.len().saturating_sub(STDERR_KEEP);
    String::from_utf8_lossy(&stderr[start..]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            timeout: Duration::from_millis(500),
            ..Limits::default()
        }
    }

    #[tokio::test]
    async fn outcomes() {
        let mut ok = command("sh");
        ok.args(["-c", "exit 0"]);
        assert!(matches!(
            run(ok, &limits()).await.unwrap(),
            Outcome::Success
        ));

        let mut failed = command("sh");
        failed.args(["-c", "echo first >&2; echo boom >&2; exit 3"]);
        match run(failed, &limits()).await.unwrap() {
            Outcome::Failed { status, stderr } => {
                assert!(status.contains('3'), "{status}");
                assert_eq!(stderr, "first\nboom");
            }
            other => panic!("{other:?}"),
        }

        let mut slow = command("sh");
        slow.args(["-c", "sleep 5"]);
        assert!(matches!(
            run(slow, &limits()).await.unwrap(),
            Outcome::TimedOut
        ));

        let missing = command("/nonexistent/agrafe-no-such-program");
        assert!(matches!(
            run(missing, &limits()).await,
            Err(Error::Spawn { .. })
        ));
    }

    #[tokio::test]
    async fn output_is_bounded() {
        let dir = std::env::temp_dir().join(format!("agrafe-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out");
        let limits = Limits {
            max_output_size: ByteSize::kib(64),
            ..limits()
        };

        let mut write = command("sh");
        write
            .args(["-c", "head -c 1048576 /dev/zero > \"$1\""])
            .arg("sh")
            .arg(&file);
        let outcome = run(write, &limits).await.unwrap();
        let written = std::fs::metadata(&file).unwrap().len();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
        assert!(
            written <= ByteSize::kib(64).as_u64(),
            "{written} bytes written"
        );
    }

    /// macOS does not enforce `RLIMIT_AS`, so the bound is Linux's alone.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn memory_is_bounded() {
        let limits = Limits {
            memory: ByteSize::mib(64),
            ..limits()
        };
        // `dd` allocates its whole block up front.
        let mut hog = command("dd");
        hog.args(["if=/dev/zero", "of=/dev/null", "bs=256M", "count=1"]);
        let outcome = run(hog, &limits).await.unwrap();
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");

        let mut small = command("dd");
        small.args(["if=/dev/zero", "of=/dev/null", "bs=1M", "count=1"]);
        assert!(matches!(
            run(small, &limits).await.unwrap(),
            Outcome::Success
        ));
    }

    #[tokio::test]
    async fn environment_is_clean() {
        let mut env = command("sh");
        env.args([
            "-c",
            "test -z \"$AGRAFE_SANDBOX_TEST\" && test -n \"$PATH\"",
        ]);
        // SAFETY: nothing else reads this variable; the configuration tests
        // set their own, so they are left alone.
        unsafe { std::env::set_var("AGRAFE_SANDBOX_TEST", "secret") };
        assert!(matches!(
            run(env, &limits()).await.unwrap(),
            Outcome::Success
        ));
    }
}
