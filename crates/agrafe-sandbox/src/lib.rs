//! Running decoders and renderers as subprocesses, confined.
//!
//! They are the most attackable code in the service, so they never run in
//! the server process: a crash, a decoder bomb or an infinite loop costs one
//! job attempt, not the process. Three layers, from the inside out:
//!
//! - `setrlimit` on the child and a wall-clock timeout in the parent, on
//!   every platform (macOS does not enforce the address space limit);
//! - a Landlock ruleset, installed by the child before `exec`: the system
//!   set is readable, the declared [`Command::readable`] paths are
//!   readable, the one [`Command::writable`] directory is writable, TCP
//!   and signals to processes outside the sandbox are refused, and nothing
//!   else exists;
//! - a seccomp denylist, installed next: no sockets (unix sockets alone
//!   on request, see [`Command::unix_sockets`]), no namespaces, no
//!   ptrace, no mounts, no reaching another process's descriptors or
//!   limits, no kernel-facing surface a process that turns bytes into
//!   pixels has any use for. Refused calls fail with `EPERM`.
//!
//! The last two are Linux only, on by default where the kernel has them
//! ([`Isolation::Auto`]), and [`Isolation::Required`] refuses to start
//! without them. [`probe`] is how a worker learns what it got. The child
//! leads a process group of its own, killed whole at the timeout, so a
//! launcher that forked does not leave what it launched behind.

#[cfg(target_os = "linux")]
mod confine;

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bytesize::ByteSize;
use serde::Deserialize;

/// What a subprocess that decodes or renders a user file may do.
/// Deserialises from the `[media.sandbox]` table of the server
/// configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Wall-clock limit; the process is killed past it.
    #[serde(with = "humantime_serde", default = "default_timeout")]
    pub timeout: Duration,
    /// Address space limit (`RLIMIT_AS`).
    #[serde(default = "default_memory")]
    pub memory: ByteSize,
    /// Largest file the process may write (`RLIMIT_FSIZE`).
    #[serde(default = "default_max_output_size")]
    pub max_output_size: ByteSize,
    /// Whether Landlock and seccomp confine the process.
    #[serde(default)]
    pub isolation: Isolation,
    /// Paths readable on top of the system set, for a host that keeps its
    /// fonts or its poppler data somewhere unusual.
    #[serde(default)]
    pub readable: Vec<PathBuf>,
}

/// Whether the kernel confines the subprocess beyond its resource limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// Landlock and seccomp where the kernel has them, logged either way.
    #[default]
    Auto,
    /// Both install, or the worker does not start. What each enforces
    /// still follows the kernel's Landlock ABI, which the boot line names.
    Required,
    /// Resource limits and the timeout only.
    Off,
}

impl Isolation {
    /// Whether a confinement layer that cannot be installed is an error.
    fn strict(self) -> bool {
        self == Isolation::Required
    }
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

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout: default_timeout(),
            memory: default_memory(),
            max_output_size: default_max_output_size(),
            isolation: Isolation::default(),
            readable: Vec::new(),
        }
    }
}

/// Bytes of stderr kept, from the end: the error comes after the warnings.
const STDERR_KEEP: usize = 4096;

/// Why a command could not be run to an [`Outcome`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The process did not start: the program is missing or not
    /// executable, or the child failed to install a confinement layer
    /// before `exec`.
    #[error("starting `{program}`: {source}")]
    Spawn {
        /// The resolved program.
        program: String,
        /// What the operating system reported.
        source: std::io::Error,
    },
    /// Waiting for the process to exit, or reading its stderr, failed.
    #[error("waiting for `{program}`: {source}")]
    Wait {
        /// The resolved program.
        program: String,
        /// What the operating system reported.
        source: std::io::Error,
    },
    /// A confinement layer `isolation = "required"` demands cannot be set
    /// up on this kernel.
    #[error("confining `{program}`: {reason}")]
    Confine {
        /// The resolved program.
        program: String,
        /// What is missing, or why the ruleset or the filter could not be
        /// built.
        reason: String,
    },
}

/// How a sandboxed process ended.
#[derive(Debug)]
pub enum Outcome {
    /// Exited with status zero.
    Success,
    /// A non-zero exit or a signal, with what the process said on stderr.
    Failed {
        /// The exit code, `None` for a signal.
        code: Option<i32>,
        /// The exit status as `std::process::ExitStatus` displays it.
        status: String,
        /// The last 4 KiB of stderr, decoded lossily and trimmed.
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

/// A command to run under the sandbox: the program, its arguments and
/// environment, and what it may touch. The child sees `PATH` and `HOME`
/// and nothing else of this process's environment, in particular no
/// secrets. The declarations are the contract; the paths in the arguments
/// are a means, and a path that is not declared does not exist for the
/// child.
pub struct Command {
    program: PathBuf,
    inner: tokio::process::Command,
    readable: Vec<PathBuf>,
    writable: Option<PathBuf>,
    unix_sockets: Option<PathBuf>,
}

impl Command {
    /// A command for `program`, a bare name looked up on `PATH` or a
    /// path, resolved now to an absolute path: the confinement has to
    /// name the file the child executes.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let program = resolve(program.as_ref());
        let mut inner = tokio::process::Command::new(&program);
        inner.env_clear();
        for &name in INHERITED {
            if let Some(value) = std::env::var_os(name) {
                inner.env(name, value);
            }
        }
        Self {
            program,
            inner,
            readable: Vec::new(),
            writable: None,
            unix_sockets: None,
        }
    }

    /// An argument for the program.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    /// Arguments for the program.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// A variable for the child's environment, on top of the inherited
    /// `PATH` and `HOME`.
    pub fn env(&mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env(name, value);
        self
    }

    /// The child's working directory, which it must also be allowed to
    /// read or write to do anything there.
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    /// A file or directory the child reads.
    pub fn readable(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.readable.push(path.into());
        self
    }

    /// The one directory the child writes; its working files, its output
    /// and, under confinement, its home and cache, so that whatever a
    /// library writes lands there and goes with it.
    pub fn writable(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.writable = Some(dir.into());
        self
    }

    /// Let the child create unix sockets, and bind them under `dir`. The
    /// seccomp filter then refuses every other socket family instead of
    /// every socket, and Landlock lets the child create and remove socket
    /// files under `dir`, and nothing else there. For a program that
    /// cannot start without one, such as LibreOffice's single-instance
    /// pipe in `/tmp`; the trade is that path-based unix sockets the child
    /// can see, such as a local Postgres's, are no longer closed to it
    /// (Landlock scopes abstract sockets only), so it is not the default.
    pub fn unix_sockets(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.unix_sockets = Some(dir.into());
        self
    }

    /// The resolved program, for messages.
    pub fn program(&self) -> &Path {
        &self.program
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// `program` as the path the child will execute: a name is searched on
/// `PATH`, a relative path is made absolute. A program that is not found
/// is left as it is, and the spawn reports it missing.
fn resolve(program: &OsStr) -> PathBuf {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() > 1 {
        return std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return path.to_path_buf();
    };
    std::env::split_paths(&paths)
        .map(|dir| dir.join(path))
        .find(|candidate| is_executable(candidate))
        .and_then(|found| std::path::absolute(&found).ok())
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Which confinement layers a spawn sets up, and whether failing to is an
/// error.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Layers {
    landlock: bool,
    seccomp: bool,
    strict: bool,
}

impl Layers {
    fn of(isolation: Isolation) -> Self {
        let on = isolation != Isolation::Off;
        Self {
            landlock: on,
            seccomp: on,
            strict: isolation.strict(),
        }
    }
}

/// What a spawn planned to install; see `confine::Plan::applied` for what
/// that does and does not say about the child.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Applied {
    landlock: bool,
    seccomp: bool,
}

/// Run `command` to completion under `config`.
pub async fn run(command: Command, config: &Config) -> Result<Outcome, Error> {
    run_with(command, config, Layers::of(config.isolation))
        .await
        .map(|(outcome, _)| outcome)
}

async fn run_with(
    mut command: Command,
    config: &Config,
    layers: Layers,
) -> Result<(Outcome, Applied), Error> {
    let program = command.program.to_string_lossy().into_owned();
    command
        .inner
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.inner.process_group(0);
    apply_limits(&mut command.inner, config);
    let applied =
        apply_confinement(&mut command, config, layers).map_err(|reason| Error::Confine {
            program: program.clone(),
            reason,
        })?;
    let child = command.inner.spawn().map_err(|source| Error::Spawn {
        program: program.clone(),
        source,
    })?;
    let pid = child.id();
    let outcome = match tokio::time::timeout(config.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => Outcome::Success,
        Ok(Ok(output)) => Outcome::Failed {
            code: output.status.code(),
            status: output.status.to_string(),
            stderr: tail(&output.stderr),
        },
        Ok(Err(source)) => return Err(Error::Wait { program, source }),
        // Dropping the child on timeout kills it; its process group goes
        // with it, for a program that is a launcher around another.
        Err(_) => {
            kill_group(pid);
            Outcome::TimedOut
        }
    };
    Ok((outcome, applied))
}

/// Kill the process group the child leads, so that a program that forked
/// (a launcher script, an office suite's wrapper) does not leave the
/// worker behind. The child itself is killed by the drop that follows.
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: `kill` on a group id is a plain syscall; the group is the
        // one `process_group(0)` created for this child, and a pid is not
        // reused while the child is unreaped.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

#[cfg(unix)]
fn apply_limits(command: &mut tokio::process::Command, config: &Config) {
    let memory = config.memory.as_u64();
    let cpu = config.timeout.as_secs().saturating_add(1);
    let output = config.max_output_size.as_u64();
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
fn apply_limits(_command: &mut tokio::process::Command, _config: &Config) {}

/// Prepare the Landlock ruleset and the seccomp filter in this process
/// and have the child install them before `exec`. Under confinement
/// `HOME` and `XDG_CACHE_HOME` move to the writable directory, so that a
/// fontconfig cache rebuild lands there and is removed with it instead of
/// failing on a home the child cannot write.
#[cfg(target_os = "linux")]
fn apply_confinement(
    command: &mut Command,
    config: &Config,
    layers: Layers,
) -> Result<Applied, String> {
    let plan = confine::Plan::new(
        &command.program,
        &command.readable,
        command.writable.as_deref(),
        command.unix_sockets.as_deref(),
        &config.readable,
        layers,
    )
    .map_err(|err| err.to_string())?;
    let applied = plan.applied();
    if applied.landlock
        && let Some(dir) = &command.writable
    {
        command.inner.env("HOME", dir).env("XDG_CACHE_HOME", dir);
    }
    let mut plan = plan;
    // SAFETY: the ruleset and the filter were built here; the child only
    // makes the `landlock_restrict_self`, `prctl` and `seccomp` calls,
    // none of which touches the heap or a lock.
    unsafe {
        command.inner.pre_exec(move || plan.install());
    }
    Ok(applied)
}

#[cfg(not(target_os = "linux"))]
fn apply_confinement(
    _command: &mut Command,
    _config: &Config,
    layers: Layers,
) -> Result<Applied, String> {
    if layers.strict {
        return Err(linux_only());
    }
    Ok(Applied::default())
}

/// Why `isolation = "required"` cannot be met on this platform.
#[cfg(not(target_os = "linux"))]
fn linux_only() -> String {
    format!(
        "isolation = \"required\" but Landlock and seccomp exist on Linux only, not {}",
        std::env::consts::OS
    )
}

fn tail(stderr: &[u8]) -> String {
    let start = stderr.len().saturating_sub(STDERR_KEEP);
    String::from_utf8_lossy(&stderr[start..]).trim().to_string()
}

/// What the sandbox confines a child with, as [`probe`] found it, for the
/// boot log: `landlock abi 4, seccomp, rlimits`, or `rlimits only
/// (macOS)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The Landlock ABI the kernel enforces, when it refused a read the
    /// probe did not grant.
    pub landlock: Option<i32>,
    /// Whether the seccomp filter installs.
    pub seccomp: bool,
    /// Why there is nothing but resource limits, when there is not.
    pub rlimits_only: Option<String>,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(reason) = &self.rlimits_only {
            return write!(f, "rlimits only ({reason})");
        }
        if let Some(abi) = self.landlock {
            write!(f, "landlock abi {abi}, ")?;
        } else {
            write!(f, "no landlock, ")?;
        }
        if self.seccomp {
            write!(f, "seccomp, ")?;
        } else {
            write!(f, "no seccomp, ")?;
        }
        write!(f, "rlimits")
    }
}

/// Why the sandbox is not fit to run a worker.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// A probe command could not be run.
    #[error(transparent)]
    Run(#[from] Error),
    /// The seccomp filter does not install, and `isolation = "required"`.
    #[error("confining `{program}`: the seccomp filter does not install: {source}")]
    Seccomp {
        /// The resolved `cat`.
        program: String,
        /// Why the child could not start under the filter.
        source: std::io::Error,
    },
    /// A read the probe granted was refused: the system set misses
    /// something this host needs, or the ruleset is wrong.
    #[error(
        "the sandbox refused `{program}` a read it was granted ({status}: {stderr}); \
         check media.sandbox.readable"
    )]
    Granted {
        /// The resolved `cat`.
        program: String,
        /// How it ended, as in [`Outcome::Failed`], or `timed out`.
        status: String,
        /// The end of what it said on stderr.
        stderr: String,
    },
    /// A read the probe did not grant went through although Landlock
    /// claimed to be enforced.
    #[error("the sandbox let `{program}` read outside the directories it was granted")]
    NotRefused {
        /// The resolved `cat`.
        program: String,
    },
    /// The probe reads a file with `cat`, which this host does not have.
    #[error("the sandbox probe needs `{program}` on PATH, and this host has none")]
    Tool {
        /// The name looked up, `cat`.
        program: String,
    },
    /// The probe's temporary directories or files could not be created.
    #[error("temporary files for the sandbox probe: {0}")]
    Io(#[from] std::io::Error),
}

/// The one tool the probe runs. Resolved before anything else, so a host
/// without it says so instead of reporting a missing layer.
const PROBE_TOOL: &str = "cat";

/// Learn what the sandbox confines a child with, and that it refuses what
/// it should: two sibling temporary directories, one granted, and `cat`
/// on a file in each, first under seccomp alone to learn whether the
/// filter installs, then under both layers. The granted read must succeed
/// and the other must fail where Landlock is enforced. With
/// [`Isolation::Required`] a layer that does not install is an error;
/// with [`Isolation::Auto`] it is reported in the [`Status`].
pub async fn probe(config: &Config) -> Result<Status, ProbeError> {
    if config.isolation == Isolation::Off {
        return Ok(Status {
            landlock: None,
            seccomp: false,
            rlimits_only: Some("isolation off".to_string()),
        });
    }
    probe_confinement(config).await
}

#[cfg(target_os = "linux")]
async fn probe_confinement(config: &Config) -> Result<Status, ProbeError> {
    let strict = config.isolation.strict();
    let tool = resolve(OsStr::new(PROBE_TOOL));
    if !tool.is_absolute() {
        return Err(ProbeError::Tool {
            program: PROBE_TOOL.to_string(),
        });
    }
    let program = tool.to_string_lossy().into_owned();

    let root = tempfile::Builder::new()
        .prefix("agrafe-sandbox-")
        .tempdir()?;
    let granted = root.path().join("granted");
    let other = root.path().join("other");
    for dir in [&granted, &other] {
        std::fs::create_dir(dir)?;
        std::fs::write(dir.join("file"), b"agrafe\n")?;
    }
    let read_granted = || {
        let mut cat = Command::new(&tool);
        cat.arg(granted.join("file")).readable(&granted);
        cat
    };

    let seccomp = {
        let only_seccomp = Layers {
            landlock: false,
            seccomp: true,
            strict: true,
        };
        match run_with(read_granted(), config, only_seccomp).await {
            Ok(_) => true,
            Err(Error::Spawn { .. }) if !strict => false,
            Err(Error::Spawn { program, source }) => {
                return Err(ProbeError::Seccomp { program, source });
            }
            Err(err) => return Err(err.into()),
        }
    };
    let layers = Layers {
        landlock: true,
        seccomp,
        strict,
    };

    let (outcome, applied) = run_with(read_granted(), config, layers).await?;
    match outcome {
        Outcome::Success => {}
        Outcome::Failed { status, stderr, .. } => {
            return Err(ProbeError::Granted {
                program,
                status,
                stderr,
            });
        }
        Outcome::TimedOut => {
            return Err(ProbeError::Granted {
                program,
                status: "timed out".to_string(),
                stderr: String::new(),
            });
        }
    }

    let mut outside = Command::new(&tool);
    outside.arg(other.join("file")).readable(&granted);
    let (outcome, _) = run_with(outside, config, layers).await?;
    let landlock = match (applied.landlock, outcome) {
        (true, Outcome::Success) => return Err(ProbeError::NotRefused { program }),
        (true, _) => confine::kernel_abi(),
        (false, _) => None,
    };

    let seccomp = applied.seccomp;
    let rlimits_only = if landlock.is_none() && !seccomp {
        Some("the kernel has neither landlock nor seccomp".to_string())
    } else {
        None
    };
    Ok(Status {
        landlock,
        seccomp,
        rlimits_only,
    })
}

#[cfg(not(target_os = "linux"))]
async fn probe_confinement(config: &Config) -> Result<Status, ProbeError> {
    if config.isolation.strict() {
        return Err(Error::Confine {
            program: PROBE_TOOL.to_string(),
            reason: linux_only(),
        }
        .into());
    }
    Ok(Status {
        landlock: None,
        seccomp: false,
        rlimits_only: Some(std::env::consts::OS.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            timeout: Duration::from_millis(500),
            ..Config::default()
        }
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("agrafe-sandbox-test-")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn programs_are_resolved() {
        assert!(Command::new("sh").program().is_absolute());
        assert!(Command::new("/bin/sh").program() == Path::new("/bin/sh"));
        assert_eq!(
            Command::new("nonexistent-agrafe-program").program(),
            Path::new("nonexistent-agrafe-program")
        );
    }

    #[tokio::test]
    async fn outcomes() {
        let mut ok = sh("exit 0");
        ok.readable("/").writable(std::env::temp_dir());
        assert!(matches!(
            run(ok, &config()).await.unwrap(),
            Outcome::Success
        ));

        let mut failed = sh("echo first >&2; echo boom >&2; exit 3");
        failed.readable("/");
        match run(failed, &config()).await.unwrap() {
            Outcome::Failed {
                code,
                status,
                stderr,
            } => {
                assert_eq!(code, Some(3), "{status}");
                assert_eq!(stderr, "first\nboom");
            }
            other => panic!("{other:?}"),
        }

        let mut slow = sh("sleep 5");
        slow.readable("/");
        assert!(matches!(
            run(slow, &config()).await.unwrap(),
            Outcome::TimedOut
        ));

        let missing = Command::new("/nonexistent/agrafe-no-such-program");
        assert!(matches!(
            run(missing, &config()).await,
            Err(Error::Spawn { .. })
        ));
    }

    /// A launcher that forked (a script, an office suite's wrapper) is
    /// killed with what it forked at the timeout, not left running.
    #[tokio::test]
    async fn timeouts_kill_the_group() {
        let work = tempdir();
        let pidfile = work.path().join("pid");
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & echo $! > \"$1\"; wait", "sh"])
            .arg(&pidfile)
            .writable(work.path());
        let config = Config {
            isolation: Isolation::Off,
            ..config()
        };
        assert!(matches!(
            run(command, &config).await.unwrap(),
            Outcome::TimedOut
        ));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Init reaps the orphan a moment after the kill; until then it is
        // a zombie, which `kill` still finds.
        let gone = || {
            // SAFETY: a null signal only checks whether the pid exists.
            let missing = unsafe { libc::kill(pid, 0) } == -1;
            missing && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        };
        for _ in 0..200 {
            if gone() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the forked `sleep` {pid} outlived the timeout");
    }

    #[tokio::test]
    async fn output_is_bounded() {
        let dir = tempdir();
        let file = dir.path().join("out");
        let config = Config {
            max_output_size: ByteSize::kib(64),
            ..config()
        };

        let mut write = sh("head -c 1048576 /dev/zero > \"$1\"");
        write.arg("sh").arg(&file).writable(dir.path());
        let outcome = run(write, &config).await.unwrap();
        let written = std::fs::metadata(&file).unwrap().len();
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
        let config = Config {
            memory: ByteSize::mib(64),
            ..config()
        };
        // `dd` allocates its whole block up front.
        let mut hog = Command::new("dd");
        hog.args(["if=/dev/zero", "of=/dev/null", "bs=256M", "count=1"]);
        let outcome = run(hog, &config).await.unwrap();
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");

        let mut small = Command::new("dd");
        small.args(["if=/dev/zero", "of=/dev/null", "bs=1M", "count=1"]);
        assert!(matches!(
            run(small, &config).await.unwrap(),
            Outcome::Success
        ));
    }

    #[tokio::test]
    async fn environment_is_clean() {
        let env = sh("test -z \"$AGRAFE_SANDBOX_TEST\" && test -n \"$PATH\"");
        // SAFETY: nothing else reads this variable; the configuration tests
        // set their own, so they are left alone.
        unsafe { std::env::set_var("AGRAFE_SANDBOX_TEST", "secret") };
        assert!(matches!(
            run(env, &config()).await.unwrap(),
            Outcome::Success
        ));
    }

    #[tokio::test]
    async fn isolation_off_is_rlimits_only() {
        let config = Config {
            isolation: Isolation::Off,
            ..config()
        };
        let status = probe(&config).await.unwrap();
        assert_eq!(status.to_string(), "rlimits only (isolation off)");
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn other_platforms_have_rlimits_only() {
        let status = probe(&config()).await.unwrap();
        assert!(status.rlimits_only.is_some(), "{status}");
        let required = Config {
            isolation: Isolation::Required,
            ..config()
        };
        let err = probe(&required).await.unwrap_err();
        assert!(err.to_string().contains("Linux only"), "{err}");
    }

    /// The configured isolation wins over a required check on another
    /// platform: `auto` and `off` run.
    #[tokio::test]
    async fn the_boot_line_names_every_layer() {
        let status = Status {
            landlock: Some(4),
            seccomp: true,
            rlimits_only: None,
        };
        assert_eq!(status.to_string(), "landlock abi 4, seccomp, rlimits");
        let status = Status {
            landlock: None,
            seccomp: true,
            rlimits_only: None,
        };
        assert_eq!(status.to_string(), "no landlock, seccomp, rlimits");
    }

    /// What is refused, on a kernel that can refuse it: the Landlock tests
    /// need both layers, the seccomp tests the filter alone. Elsewhere they
    /// skip with a message, unless `AGRAFE_SANDBOX_REQUIRED=1`, which CI
    /// sets so that a regression is not a green empty suite. Docker's
    /// default profile already refuses `unshare`, so the namespace test
    /// discriminates only on a bare host.
    #[cfg(target_os = "linux")]
    mod refusals {
        use super::*;

        /// The configuration the tests run under when the sandbox has the
        /// layers `need` asks for, or `None` with a message when this kernel
        /// does not and the tests are not required.
        async fn confined_with(need: fn(&Status) -> bool) -> Option<Config> {
            let config = config();
            let status = probe(&config).await.unwrap();
            if need(&status) {
                return Some(config);
            }
            if std::env::var_os("AGRAFE_SANDBOX_REQUIRED").is_some_and(|v| v == "1") {
                panic!("AGRAFE_SANDBOX_REQUIRED=1 but the sandbox is {status}");
            }
            eprintln!("skipped: the sandbox is {status}");
            None
        }

        /// Both layers, for what Landlock refuses.
        async fn confined() -> Option<Config> {
            confined_with(|status| status.landlock.is_some() && status.seccomp).await
        }

        /// The seccomp filter alone, for what it refuses: these run on a
        /// kernel without Landlock too.
        async fn filtered() -> Option<Config> {
            confined_with(|status| status.seccomp).await
        }

        fn has(program: &str) -> bool {
            Command::new(program).program().is_absolute()
        }

        #[tokio::test]
        async fn required_confines() {
            let Some(config) = confined().await else {
                return;
            };
            let required = Config {
                isolation: Isolation::Required,
                ..config
            };
            let status = probe(&required).await.unwrap();
            assert!(status.landlock.is_some() && status.seccomp, "{status}");
        }

        #[tokio::test]
        async fn reads_outside_are_refused() {
            let Some(config) = confined().await else {
                return;
            };
            let dir = tempdir();
            let secret = dir.path().join("secret");
            std::fs::write(&secret, b"key").unwrap();
            let work = tempdir();
            std::fs::write(work.path().join("input"), b"input").unwrap();

            let mut cat = Command::new("cat");
            cat.arg(&secret).writable(work.path());
            match run(cat, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("Permission denied"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }

            let mut cat = Command::new("cat");
            cat.arg(work.path().join("input")).writable(work.path());
            assert!(matches!(run(cat, &config).await.unwrap(), Outcome::Success));

            // The parent's environment is not where the secrets live, but a
            // configuration file next to the binary would be: a directory
            // is not listable either.
            let beside = tempdir();
            std::fs::write(beside.path().join("agrafe.toml"), b"[keys]").unwrap();
            let mut ls = Command::new("ls");
            ls.arg(beside.path()).writable(work.path());
            assert!(matches!(
                run(ls, &config).await.unwrap(),
                Outcome::Failed { .. }
            ));
        }

        #[tokio::test]
        async fn writes_outside_are_refused() {
            let Some(config) = confined().await else {
                return;
            };
            let work = tempdir();
            let elsewhere = tempdir();

            let mut write = sh("echo x > \"$1\"");
            write
                .arg("sh")
                .arg(elsewhere.path().join("out"))
                .writable(work.path());
            assert!(matches!(
                run(write, &config).await.unwrap(),
                Outcome::Failed { .. }
            ));
            assert!(!elsewhere.path().join("out").exists());

            let mut write = sh("echo x > \"$1\" && mkdir \"$2\" && mv \"$1\" \"$2\"");
            write
                .arg("sh")
                .arg(work.path().join("out"))
                .arg(work.path().join("sub"))
                .writable(work.path());
            assert!(matches!(
                run(write, &config).await.unwrap(),
                Outcome::Success
            ));
            assert!(work.path().join("sub/out").exists());

            // A readable path is read-only.
            let mut write = sh("echo x > \"$1\"");
            write
                .arg("sh")
                .arg(elsewhere.path().join("out"))
                .readable(elsewhere.path())
                .writable(work.path());
            assert!(matches!(
                run(write, &config).await.unwrap(),
                Outcome::Failed { .. }
            ));
        }

        #[tokio::test]
        async fn connects_are_refused() {
            let Some(config) = filtered().await else {
                return;
            };
            if !has("bash") {
                eprintln!("skipped: no bash");
                return;
            }
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let work = tempdir();

            let mut connect = Command::new("bash");
            connect
                .args(["-c", "exec 3<>/dev/tcp/127.0.0.1/$1"])
                .arg("bash")
                .arg(port.to_string())
                .writable(work.path());
            match run(connect, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("not permitted"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(
                listener.accept().map(|_| ()).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }

        /// A socket of any family, unix included, is refused unless the
        /// command asks for unix sockets, and then every other family
        /// still is.
        const SOCKETS: &str = r#"
import socket, sys
try:
    socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sys.exit(10)
except PermissionError:
    pass
socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
"#;

        #[tokio::test]
        async fn unix_sockets_are_refused_by_default() {
            let Some(config) = filtered().await else {
                return;
            };
            if !has("python3") {
                eprintln!("skipped: no python3");
                return;
            }
            let work = tempdir();
            let mut sockets = Command::new("python3");
            sockets.args(["-I", "-c", SOCKETS]).writable(work.path());
            match run(sockets, &config).await.unwrap() {
                Outcome::Failed { code, stderr, .. } => {
                    assert_ne!(code, Some(10), "an inet socket was allowed");
                    assert!(stderr.contains("PermissionError"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
        }

        /// With unix sockets allowed, a bind lands where granted and
        /// nowhere else, and inet sockets stay refused.
        #[tokio::test]
        async fn unix_sockets_bind_where_granted() {
            let Some(config) = confined().await else {
                return;
            };
            if !has("python3") {
                eprintln!("skipped: no python3");
                return;
            }
            let work = tempdir();
            let pipes = tempdir();
            let elsewhere = tempdir();
            let script = format!(
                "{SOCKETS}\n\
                 import os\n\
                 s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n\
                 s.bind(sys.argv[1] + '/pipe')\n\
                 s.listen(1)\n\
                 os.unlink(sys.argv[1] + '/pipe')\n\
                 try:\n\
                 \tsocket.socket(socket.AF_UNIX, socket.SOCK_STREAM).bind(sys.argv[2] + '/pipe')\n\
                 \tsys.exit(11)\n\
                 except PermissionError:\n\
                 \tpass\n"
            );
            let mut sockets = Command::new("python3");
            sockets
                .args(["-I", "-c", &script])
                .arg(pipes.path())
                .arg(elsewhere.path())
                .writable(work.path())
                .unix_sockets(pipes.path());
            match run(sockets, &config).await.unwrap() {
                Outcome::Success => {}
                other => panic!("{other:?}"),
            }
            assert!(
                !pipes.path().join("pipe").exists(),
                "the pipe was not removed"
            );
            assert!(!elsewhere.path().join("pipe").exists());
        }

        #[tokio::test]
        async fn namespaces_are_refused() {
            let Some(config) = filtered().await else {
                return;
            };
            if !has("unshare") {
                eprintln!("skipped: no unshare");
                return;
            }
            let work = tempdir();
            let mut unshare = Command::new("unshare");
            unshare.args(["--user", "true"]).writable(work.path());
            match run(unshare, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("not permitted"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
        }

        /// The child shares this process's uid, so without the signal scope
        /// `kill` would reach the worker. The scope is ABI 6 (kernel 6.12);
        /// on an older kernel the test skips, unless required.
        #[tokio::test]
        async fn signals_outside_are_refused() {
            let Some(config) = confined().await else {
                return;
            };
            let status = probe(&config).await.unwrap();
            if status.landlock.is_some_and(|abi| abi < 6) {
                let message = format!("the kernel has no signal scope ({status})");
                assert!(
                    std::env::var_os("AGRAFE_SANDBOX_REQUIRED").is_none_or(|v| v != "1"),
                    "AGRAFE_SANDBOX_REQUIRED=1 but {message}"
                );
                eprintln!("skipped: {message}");
                return;
            }
            let work = tempdir();
            // `kill -0` only asks whether the signal may be sent.
            let mut kill = sh("kill -0 $PPID");
            kill.writable(work.path());
            match run(kill, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("not permitted"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
            // Its own children are inside the domain.
            let mut own = sh("sleep 5 & kill $!");
            own.writable(work.path());
            assert!(matches!(run(own, &config).await.unwrap(), Outcome::Success));
        }

        /// `prlimit64` on another process is refused; on the process itself
        /// it is how glibc reads and sets limits, so it keeps working.
        #[tokio::test]
        async fn limits_of_other_processes_are_refused() {
            let Some(config) = filtered().await else {
                return;
            };
            if !has("prlimit") {
                eprintln!("skipped: no prlimit");
                return;
            }
            let work = tempdir();
            let mut other = sh("prlimit --pid $PPID --nofile");
            other.writable(work.path());
            match run(other, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("not permitted"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
            let mut own = sh("prlimit --nofile > /dev/null && ulimit -n 64");
            own.writable(work.path());
            assert!(matches!(run(own, &config).await.unwrap(), Outcome::Success));
        }

        /// Threads still work: glibc's `clone3` is answered `ENOSYS`, not
        /// `EPERM`, so it falls back to `clone`.
        #[tokio::test]
        async fn threads_still_work() {
            let Some(config) = filtered().await else {
                return;
            };
            let work = tempdir();
            // `sort --parallel` spawns threads for a large enough input;
            // `xargs -P` forks, which is a `clone` without namespace flags.
            let mut threads = sh("seq 1 200000 | sort --parallel=4 -S 64M > /dev/null && \
                                  seq 1 4 | xargs -P 4 -I{} true");
            threads.writable(work.path());
            match run(threads, &config).await.unwrap() {
                Outcome::Success => {}
                other => panic!("{other:?}"),
            }
        }

        /// A ruleset and a filter add well under a millisecond per spawn,
        /// so the spawn stays cheap next to the tool's own startup.
        #[tokio::test]
        async fn spawns_stay_cheap() {
            let Some(config) = confined().await else {
                return;
            };
            let work = tempdir();
            let started = std::time::Instant::now();
            for _ in 0..20 {
                let mut t = Command::new("true");
                t.writable(work.path());
                assert!(matches!(run(t, &config).await.unwrap(), Outcome::Success));
            }
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_secs(4),
                "20 spawns took {elapsed:?}"
            );
        }
    }
}
