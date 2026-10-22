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
//!   ptrace, no mounts, no session or process group of the child's own,
//!   no reaching another process's descriptors, limits, scheduling or
//!   memory, no
//!   kernel-facing surface a process that turns bytes into pixels has
//!   any use for. Refused calls fail with `EPERM`.
//!
//! The last two are Linux only, on by default where the kernel has them
//! ([`Isolation::Auto`]), and [`Isolation::Required`] refuses to start
//! without them. [`probe`] is how a worker learns what it got. The child
//! leads a process group of its own, killed whole at the timeout, so a
//! launcher that forked does not leave what it launched behind.
//!
//! On a host that grants unprivileged user namespaces, [`Config::bubblewrap`]
//! puts bubblewrap around the same child: a mount namespace where the
//! configuration file does not exist rather than is denied, a pid
//! namespace, a network namespace with loopback alone, a fresh bounded
//! `/tmp` and a uid of the child's own. The rlimits stay, the seccomp
//! filter is handed to `bwrap` over a descriptor, and Landlock yields to
//! the mount namespace, since a Landlock domain forbids the `mount`
//! bubblewrap needs. Asked for and impossible, the sandbox is an error.

#[cfg(target_os = "linux")]
mod bubblewrap;
#[cfg(target_os = "linux")]
mod confine;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bytesize::ByteSize;
use serde::Deserialize;
use tracing::Instrument as _;

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
    /// Processes and threads the process's uid may have at once
    /// (`RLIMIT_NPROC`), which bounds a fork bomb. The kernel counts
    /// every process and thread of the uid, the worker's own included,
    /// so the default suits a uid of the worker's own and a shared one
    /// raises it.
    #[serde(default = "default_max_processes")]
    pub max_processes: u64,
    /// Whether Landlock and seccomp confine the process.
    #[serde(default)]
    pub isolation: Isolation,
    /// Paths readable on top of the system set, for a host that keeps its
    /// fonts or its poppler data somewhere unusual.
    #[serde(default)]
    pub readable: Vec<PathBuf>,
    /// The `bwrap` binary, when bubblewrap wraps every subprocess in a
    /// mount, pid and network namespace of its own. Unset by default; it
    /// needs a host that grants unprivileged user namespaces, and on
    /// Ubuntu that is the packaged `/usr/bin/bwrap` alone, which its
    /// AppArmor profile attaches to by path. Set and not working, the
    /// worker refuses to start, whatever `isolation` says short of `off`.
    #[serde(default)]
    pub bubblewrap: Option<PathBuf>,
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
fn default_max_processes() -> u64 {
    1024
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout: default_timeout(),
            memory: default_memory(),
            max_output_size: default_max_output_size(),
            max_processes: default_max_processes(),
            isolation: Isolation::default(),
            readable: Vec::new(),
            bubblewrap: None,
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
    /// bubblewrap could not set the sandbox up around `program`: the host
    /// grants it no user namespace, or a path it was to bind is missing.
    /// A host matter, never the file's, so a job that meets it is retried
    /// rather than the blob marked failed.
    #[error("bubblewrap could not start the sandbox for `{program}`: {message}")]
    Bubblewrap {
        /// The resolved program.
        program: String,
        /// What `bwrap` said on stderr.
        message: String,
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
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    current_dir: Option<PathBuf>,
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
        let envs = INHERITED
            .iter()
            .filter_map(|&name| std::env::var_os(name).map(|value| (name.into(), value)))
            .collect();
        Self {
            program,
            args: Vec::new(),
            envs,
            current_dir: None,
            readable: Vec::new(),
            writable: None,
            unix_sockets: None,
        }
    }

    /// An argument for the program.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Arguments for the program.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    /// A variable for the child's environment, on top of the inherited
    /// `PATH` and `HOME`.
    pub fn env(&mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.envs
            .push((name.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// The child's working directory, which it must also be allowed to
    /// read or write to do anything there.
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_path_buf());
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
    /// Under bubblewrap the sockets go in the sandbox's private `/tmp`,
    /// whatever `dir` says: nothing of the host is bound for them.
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
        write!(f, "{:?}", self.program)?;
        for arg in &self.args {
            write!(f, " {arg:?}")?;
        }
        Ok(())
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
/// error. Under bubblewrap Landlock is off, since a Landlock domain
/// forbids the `mount` `bwrap` needs and the mount namespace covers what
/// the ruleset covered, and every failure is an error: asked for and
/// impossible, the sandbox does not run unconfined. A spawn hands back
/// the layers it planned after the kernel had its say; see
/// `confine::Plan::layers` for what that does and does not say about
/// the child.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Layers {
    landlock: bool,
    seccomp: bool,
    bubblewrap: bool,
    strict: bool,
}

impl Layers {
    fn of(config: &Config) -> Self {
        if config.isolation == Isolation::Off {
            return Self {
                landlock: false,
                seccomp: false,
                bubblewrap: false,
                strict: false,
            };
        }
        let bubblewrap = config.bubblewrap.is_some();
        Self {
            landlock: !bubblewrap,
            seccomp: true,
            bubblewrap,
            strict: bubblewrap || config.isolation.strict(),
        }
    }
}

/// Run `command` to completion under `config`, in a `sandbox` span that
/// names the program, never its arguments (paths of the files it reads),
/// and records how it ended.
pub async fn run(command: Command, config: &Config) -> Result<Outcome, Error> {
    let program = command
        .program
        .file_name()
        .unwrap_or(command.program.as_os_str());
    let span = tracing::info_span!(
        "sandbox",
        program = %program.to_string_lossy(),
        outcome = tracing::field::Empty,
    );
    let result = run_with(command, config, Layers::of(config))
        .instrument(span.clone())
        .await
        .map(|(outcome, _)| outcome);
    span.record(
        "outcome",
        match &result {
            Ok(Outcome::Success) => "success",
            Ok(Outcome::Failed { .. }) => "failed",
            Ok(Outcome::TimedOut) => "timed_out",
            Err(_) => "error",
        },
    );
    result
}

async fn run_with(
    command: Command,
    config: &Config,
    layers: Layers,
) -> Result<(Outcome, Layers), Error> {
    let program = command.program.to_string_lossy().into_owned();
    let (mut inner, planned) =
        prepare(&command, config, layers).map_err(|reason| Error::Confine {
            program: program.clone(),
            reason,
        })?;
    inner
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    inner.process_group(0);
    let mut child = inner.spawn().map_err(|source| Error::Spawn {
        program: program.clone(),
        source,
    })?;
    let pid = child.id();
    // The exit and the end of stderr, read as it comes so that a tool
    // that floods it costs this process 4 KiB, not the flood.
    let stderr = child.stderr.take();
    let waited = async {
        let (status, stderr) = tokio::join!(child.wait(), stderr_tail(stderr));
        Ok::<_, std::io::Error>((status?, stderr?))
    };
    let outcome = match tokio::time::timeout(config.timeout, waited).await {
        Ok(Ok((status, _))) if status.success() => Outcome::Success,
        // CPU time ran out before the wall clock did, which a process on
        // several threads does: a timeout all the same, not a fault of the
        // file. A launcher reports its child's death its own way, so this
        // tells a direct tool's; bubblewrap reports it as the shell does,
        // 128 plus the signal.
        Ok(Ok((status, _))) if cpu_exhausted(&status, layers.bubblewrap) => Outcome::TimedOut,
        Ok(Ok((status, stderr))) => {
            let stderr = tail(&stderr);
            // `bwrap` failed before the program ran: a host matter.
            if layers.bubblewrap
                && status.code() == Some(1)
                && let Some(err) = bubblewrap_failure(program.clone(), &stderr)
            {
                return Err(err);
            }
            Outcome::Failed {
                code: status.code(),
                status: status.to_string(),
                stderr,
            }
        }
        // The child may still be running: it goes with its group, as at
        // the timeout.
        Ok(Err(source)) => {
            kill_group(pid);
            return Err(Error::Wait { program, source });
        }
        // The child is killed with its process group, for a program that
        // is a launcher around another, and dropped after.
        Err(_) => {
            kill_group(pid);
            Outcome::TimedOut
        }
    };
    Ok((outcome, planned))
}

/// The last [`STDERR_KEEP`] bytes of what the child writes on stderr,
/// read as it is written: the pipe is drained to the end so that the
/// child never blocks on it, and nothing past the tail is kept.
async fn stderr_tail(stderr: Option<tokio::process::ChildStderr>) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let Some(mut stderr) = stderr else {
        return Ok(Vec::new());
    };
    let mut kept = Vec::with_capacity(2 * STDERR_KEEP);
    let mut chunk = [0u8; STDERR_KEEP];
    loop {
        let read = stderr.read(&mut chunk).await?;
        if read == 0 {
            return Ok(kept);
        }
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > STDERR_KEEP {
            kept.drain(..kept.len() - STDERR_KEEP);
        }
    }
}

/// Kill the process group the child leads, so that a program that forked
/// (a launcher script, an office suite's wrapper) does not leave the
/// worker behind. The child itself is killed by the drop that follows.
/// Under bubblewrap the group holds `bwrap` alone, since its pid 1 starts
/// a session of its own (`--new-session`); there `--die-with-parent`
/// does the work, killing pid 1 when `bwrap` dies, and the kernel takes
/// the pid namespace down with it.
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

/// Whether the process died of `SIGXCPU`, the CPU time limit. `bwrap`,
/// the init of its pid namespace, reports a child the kernel killed as an
/// exit of 128 plus the signal, the shell's convention.
#[cfg(unix)]
fn cpu_exhausted(status: &std::process::ExitStatus, bubblewrap: bool) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGXCPU)
        || (bubblewrap && status.code() == Some(128 + libc::SIGXCPU))
}

#[cfg(not(unix))]
fn cpu_exhausted(_status: &std::process::ExitStatus, _bubblewrap: bool) -> bool {
    false
}

/// The error behind an exit of 1 under bubblewrap whose stderr is
/// `bwrap`'s own: nothing of the program's comes before it, since the
/// program never ran. A program it could not execute is reported as a
/// spawn failure, as it is without bubblewrap.
fn bubblewrap_failure(program: String, stderr: &str) -> Option<Error> {
    const PREFIX: &str = "bwrap: ";
    let message = stderr.strip_prefix(PREFIX)?;
    if let Some(rest) = message.strip_prefix("execvp ")
        && let Some((_, cause)) = rest.split_once(": ")
    {
        return Some(Error::Spawn {
            program,
            source: std::io::Error::other(cause.trim().to_string()),
        });
    }
    Some(Error::Bubblewrap {
        program,
        message: stderr.to_string(),
    })
}

/// Seconds of CPU time past the soft limit before the hard one: the soft
/// limit sends `SIGXCPU`, which names the cause, and the hard limit sends
/// `SIGKILL` to a process that caught it.
#[cfg(unix)]
const CPU_GRACE: u64 = 1;

#[cfg(unix)]
fn apply_limits(command: &mut tokio::process::Command, config: &Config) {
    let memory = config.memory.as_u64();
    let cpu = config.timeout.as_secs().saturating_add(1);
    let output = config.max_output_size.as_u64();
    let processes = config.max_processes;
    let limit = |value: u64| libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // With the soft and the hard limit equal the kernel checks the hard one
    // first and sends `SIGKILL`, which says nothing; a hard limit one
    // second past the soft one makes the death `SIGXCPU`.
    let cpu = libc::rlimit {
        rlim_cur: cpu as libc::rlim_t,
        rlim_max: cpu.saturating_add(CPU_GRACE) as libc::rlim_t,
    };
    // SAFETY: `setrlimit` is async-signal-safe and touches nothing shared
    // with the parent. A limit the hard limit forbids is left as inherited,
    // which is stricter.
    unsafe {
        command.pre_exec(move || {
            libc::setrlimit(libc::RLIMIT_AS, &limit(memory));
            libc::setrlimit(libc::RLIMIT_DATA, &limit(memory));
            libc::setrlimit(libc::RLIMIT_CPU, &cpu);
            libc::setrlimit(libc::RLIMIT_FSIZE, &limit(output));
            libc::setrlimit(libc::RLIMIT_NPROC, &limit(processes));
            libc::setrlimit(libc::RLIMIT_CORE, &limit(0));
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn apply_limits(_command: &mut tokio::process::Command, _config: &Config) {}

/// The process to spawn for `command`: the program with its arguments,
/// environment and limits, confined as `layers` say. The limits go first
/// among the child's `pre_exec` steps, the confinement after.
#[cfg(target_os = "linux")]
fn prepare(
    command: &Command,
    config: &Config,
    layers: Layers,
) -> Result<(tokio::process::Command, Layers), String> {
    if layers.bubblewrap {
        return prepare_bubblewrap(command, config, layers);
    }
    let mut inner = plain(command);
    apply_limits(&mut inner, config);
    let planned = apply_confinement(command, &mut inner, config, layers)?;
    Ok((inner, planned))
}

#[cfg(not(target_os = "linux"))]
fn prepare(
    command: &Command,
    config: &Config,
    layers: Layers,
) -> Result<(tokio::process::Command, Layers), String> {
    if layers.strict {
        return Err(linux_only());
    }
    let mut inner = plain(command);
    apply_limits(&mut inner, config);
    Ok((
        inner,
        Layers {
            landlock: false,
            seccomp: false,
            ..layers
        },
    ))
}

/// The program itself, with its arguments, environment and working
/// directory, and nothing of this process's environment but what the
/// command carries.
fn plain(command: &Command) -> tokio::process::Command {
    let mut inner = tokio::process::Command::new(&command.program);
    inner.env_clear();
    inner.args(&command.args);
    inner.envs(command.envs.iter().map(|(name, value)| (name, value)));
    if let Some(dir) = &command.current_dir {
        inner.current_dir(dir);
    }
    inner
}

/// Prepare the Landlock ruleset and the seccomp filter in this process
/// and have the child install them before `exec`. Under confinement
/// `HOME` and `XDG_CACHE_HOME` move to the writable directory, so that a
/// fontconfig cache rebuild lands there and is removed with it instead of
/// failing on a home the child cannot write.
#[cfg(target_os = "linux")]
fn apply_confinement(
    command: &Command,
    inner: &mut tokio::process::Command,
    config: &Config,
    layers: Layers,
) -> Result<Layers, String> {
    let mut plan =
        confine::Plan::new(command, &config.readable, layers).map_err(|err| err.to_string())?;
    let planned = plan.layers();
    if planned.landlock {
        home_in_workdir(command, inner);
    }
    // SAFETY: the ruleset and the filter were built here; the child only
    // makes the `landlock_restrict_self`, `prctl` and `seccomp` calls,
    // none of which touches the heap or a lock.
    unsafe {
        inner.pre_exec(move || plan.install());
    }
    Ok(planned)
}

/// `bwrap` around the program, with the system set, the declared paths
/// and the seccomp filters it installs in the child after the namespaces
/// are set up. The rlimits are set on `bwrap` and inherited unchanged.
#[cfg(target_os = "linux")]
fn prepare_bubblewrap(
    command: &Command,
    config: &Config,
    layers: Layers,
) -> Result<(tokio::process::Command, Layers), String> {
    let bwrap = config
        .bubblewrap
        .as_deref()
        .map(|path| resolve(path.as_os_str()))
        .ok_or_else(|| "bubblewrap is not configured".to_string())?;
    let filters = if layers.seccomp {
        match confine::filters(command.unix_sockets.is_some()) {
            Ok(filters) => Some(filters),
            Err(err) if layers.strict => {
                return Err(confine::Error::Seccomp(err).to_string());
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let wrapped = bubblewrap::wrap(
        command,
        &config.readable,
        filters.as_ref().map(|f| f.as_slice()),
    )
    .map_err(|err| format!("preparing the seccomp filters for bwrap: {err}"))?;
    let mut inner = tokio::process::Command::new(&bwrap);
    inner.env_clear();
    inner.args(&wrapped.args);
    inner.envs(command.envs.iter().map(|(name, value)| (name, value)));
    home_in_workdir(command, &mut inner);
    apply_limits(&mut inner, config);
    let descriptors = wrapped.descriptors;
    // SAFETY: clearing close-on-exec on descriptors this process opened
    // is one `fcntl` each, with nothing shared touched.
    unsafe {
        inner.pre_exec(move || {
            for descriptor in &descriptors {
                descriptor.inherit()?;
            }
            Ok(())
        });
    }
    Ok((
        inner,
        Layers {
            landlock: false,
            seccomp: filters.is_some(),
            ..layers
        },
    ))
}

/// `HOME` and `XDG_CACHE_HOME` in the writable directory, under
/// confinement, when there is one.
#[cfg(target_os = "linux")]
fn home_in_workdir(command: &Command, inner: &mut tokio::process::Command) {
    if let Some(dir) = &command.writable {
        inner.env("HOME", dir).env("XDG_CACHE_HOME", dir);
    }
}

/// Why `isolation = "required"` or `bubblewrap` cannot be met on this
/// platform.
#[cfg(not(target_os = "linux"))]
fn linux_only() -> String {
    format!(
        "isolation = \"required\" or bubblewrap asked for, but Landlock, seccomp and \
         bubblewrap exist on Linux only, not {}",
        std::env::consts::OS
    )
}

fn tail(stderr: &[u8]) -> String {
    let start = stderr.len().saturating_sub(STDERR_KEEP);
    String::from_utf8_lossy(&stderr[start..]).trim().to_string()
}

/// What the sandbox confines a child with, as [`probe`] found it, for the
/// boot log: `landlock abi 4, seccomp, rlimits`, `bubblewrap 0.11.1,
/// seccomp, rlimits`, or `rlimits only (macOS)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The Landlock ABI the kernel enforces, when it refused a read the
    /// probe did not grant.
    pub landlock: Option<i32>,
    /// Whether the seccomp filter installs.
    pub seccomp: bool,
    /// The bubblewrap version, when it wraps the child: its mount
    /// namespace then stands in for Landlock.
    pub bubblewrap: Option<String>,
    /// Why there is nothing but resource limits, when there is not.
    pub rlimits_only: Option<String>,
}

impl Status {
    /// Whether the child's filesystem is confined to what it was granted,
    /// by Landlock or by bubblewrap's mount namespace.
    pub fn confines_filesystem(&self) -> bool {
        self.landlock.is_some() || self.bubblewrap.is_some()
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(reason) = &self.rlimits_only {
            return write!(f, "rlimits only ({reason})");
        }
        if let Some(version) = &self.bubblewrap {
            write!(f, "bubblewrap {version}, ")?;
        } else if let Some(abi) = self.landlock {
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
    /// `media.sandbox.bubblewrap` is set and `bwrap` cannot set a sandbox
    /// up on this host.
    #[error(
        "bubblewrap `{program}` cannot start a sandbox: {reason}; it needs an unprivileged \
         user namespace, which Ubuntu's AppArmor grants to the packaged /usr/bin/bwrap \
         alone and a default Docker container refuses"
    )]
    Bubblewrap {
        /// The configured `bwrap`.
        program: String,
        /// What it said, or why it could not be run.
        reason: String,
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
            bubblewrap: None,
            rlimits_only: Some("isolation off".to_string()),
        });
    }
    probe_confinement(config).await
}

/// The probe's fixture: `cat`, and two sibling directories with a file
/// in each, of which the commands grant one.
#[cfg(target_os = "linux")]
struct Fixture {
    tool: PathBuf,
    _root: tempfile::TempDir,
    granted: PathBuf,
    other: PathBuf,
}

#[cfg(target_os = "linux")]
impl Fixture {
    fn new() -> Result<Self, ProbeError> {
        let tool = resolve(OsStr::new(PROBE_TOOL));
        if !tool.is_absolute() {
            return Err(ProbeError::Tool {
                program: PROBE_TOOL.to_string(),
            });
        }
        let root = tempfile::Builder::new()
            .prefix("agrafe-sandbox-")
            .tempdir()?;
        let granted = root.path().join("granted");
        let other = root.path().join("other");
        for dir in [&granted, &other] {
            std::fs::create_dir(dir)?;
            std::fs::write(dir.join("file"), b"agrafe\n")?;
        }
        Ok(Self {
            tool,
            _root: root,
            granted,
            other,
        })
    }

    fn program(&self) -> String {
        self.tool.to_string_lossy().into_owned()
    }

    /// `cat` on the granted file.
    fn read_granted(&self) -> Command {
        let mut cat = Command::new(&self.tool);
        cat.arg(self.granted.join("file")).readable(&self.granted);
        cat
    }

    /// `cat` on the other file, with the granted directory alone granted.
    fn read_other(&self) -> Command {
        let mut cat = Command::new(&self.tool);
        cat.arg(self.other.join("file")).readable(&self.granted);
        cat
    }

    /// The granted read under `layers`, which must succeed: otherwise the
    /// system set misses something this host needs, or the sandbox is
    /// wrong.
    async fn check_granted(&self, config: &Config, layers: Layers) -> Result<Layers, ProbeError> {
        let (outcome, planned) = run_with(self.read_granted(), config, layers)
            .await
            .map_err(|err| bubblewrap_probe_error(config, err))?;
        match outcome {
            Outcome::Success => Ok(planned),
            Outcome::Failed { status, stderr, .. } => Err(ProbeError::Granted {
                program: self.program(),
                status,
                stderr,
            }),
            Outcome::TimedOut => Err(ProbeError::Granted {
                program: self.program(),
                status: "timed out".to_string(),
                stderr: String::new(),
            }),
        }
    }

    /// The other read under `layers`, which must fail.
    async fn check_refused(&self, config: &Config, layers: Layers) -> Result<(), ProbeError> {
        let (outcome, _) = run_with(self.read_other(), config, layers)
            .await
            .map_err(|err| bubblewrap_probe_error(config, err))?;
        match outcome {
            Outcome::Success => Err(ProbeError::NotRefused {
                program: self.program(),
            }),
            _ => Ok(()),
        }
    }
}

/// A `bwrap` failure during the probe is the host's configuration, and
/// reported as such, naming the configured `bwrap`.
#[cfg(target_os = "linux")]
fn bubblewrap_probe_error(config: &Config, err: Error) -> ProbeError {
    match err {
        Error::Bubblewrap { message, .. } => ProbeError::Bubblewrap {
            program: config
                .bubblewrap
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            reason: message,
        },
        err => err.into(),
    }
}

#[cfg(target_os = "linux")]
async fn probe_confinement(config: &Config) -> Result<Status, ProbeError> {
    let fixture = Fixture::new()?;
    if let Some(bwrap) = &config.bubblewrap {
        return probe_bubblewrap(config, bwrap, &fixture).await;
    }
    let strict = config.isolation.strict();

    let seccomp = {
        let only_seccomp = Layers {
            landlock: false,
            seccomp: true,
            bubblewrap: false,
            strict: true,
        };
        match run_with(fixture.read_granted(), config, only_seccomp).await {
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
        bubblewrap: false,
        strict,
    };

    let planned = fixture.check_granted(config, layers).await?;
    let landlock = if planned.landlock {
        fixture.check_refused(config, layers).await?;
        confine::kernel_abi()
    } else {
        None
    };

    let seccomp = planned.seccomp;
    let rlimits_only = if landlock.is_none() && !seccomp {
        Some("the kernel has neither landlock nor seccomp".to_string())
    } else {
        None
    };
    Ok(Status {
        landlock,
        seccomp,
        bubblewrap: None,
        rlimits_only,
    })
}

/// The same two reads through `bwrap`, which must start, with the seccomp
/// filters it installs: a `bwrap` that cannot set the sandbox up is a
/// configuration failure in every mode, since the operator asked for it.
#[cfg(target_os = "linux")]
async fn probe_bubblewrap(
    config: &Config,
    bwrap: &Path,
    fixture: &Fixture,
) -> Result<Status, ProbeError> {
    let version = bubblewrap::version(bwrap, config.timeout)
        .await
        .map_err(|reason| ProbeError::Bubblewrap {
            program: bwrap.display().to_string(),
            reason,
        })?;
    let layers = Layers {
        landlock: false,
        seccomp: true,
        bubblewrap: true,
        strict: true,
    };
    let planned = fixture.check_granted(config, layers).await?;
    fixture.check_refused(config, layers).await?;
    Ok(Status {
        landlock: None,
        seccomp: planned.seccomp,
        bubblewrap: Some(version),
        rlimits_only: None,
    })
}

#[cfg(not(target_os = "linux"))]
async fn probe_confinement(config: &Config) -> Result<Status, ProbeError> {
    if config.isolation.strict() || config.bubblewrap.is_some() {
        return Err(Error::Confine {
            program: PROBE_TOOL.to_string(),
            reason: linux_only(),
        }
        .into());
    }
    Ok(Status {
        landlock: None,
        seccomp: false,
        bubblewrap: None,
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

    /// CPU time is bounded by the wall-clock limit too, summed over the
    /// threads, and running out of it is a timeout, not a fault of the
    /// file: four threads hashing exhaust the limit in a quarter of the
    /// wall-clock time.
    #[tokio::test]
    async fn cpu_time_is_a_timeout() {
        if Command::new("python3").program().is_relative() {
            eprintln!("skipped: no python3");
            return;
        }
        let config = Config {
            timeout: Duration::from_secs(5),
            ..config()
        };
        let mut burn = Command::new("python3");
        burn.args([
            "-I",
            "-c",
            "import hashlib, threading, time\n\
             def burn():\n\
             \twhile True:\n\
             \t\thashlib.sha256(bytes(10_000_000)).digest()\n\
             for _ in range(4):\n\
             \tthreading.Thread(target=burn, daemon=True).start()\n\
             time.sleep(60)\n",
        ]);
        let started = std::time::Instant::now();
        let outcome = run(burn, &config).await.unwrap();
        assert!(matches!(outcome, Outcome::TimedOut), "{outcome:?}");
        assert!(
            started.elapsed() < config.timeout,
            "the wall clock ran out first, after {:?}",
            started.elapsed()
        );
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

    /// The process count is bounded too, per uid, which is how the kernel
    /// counts it.
    #[tokio::test]
    async fn processes_are_bounded() {
        if Command::new("python3").program().is_relative() {
            eprintln!("skipped: no python3");
            return;
        }
        let config = Config {
            max_processes: 777,
            ..config()
        };
        let mut limit = Command::new("python3");
        limit.args([
            "-I",
            "-c",
            "import resource, sys\n\
             sys.exit(0 if resource.getrlimit(resource.RLIMIT_NPROC) == (777, 777) else 1)\n",
        ]);
        let outcome = run(limit, &config).await.unwrap();
        assert!(matches!(outcome, Outcome::Success), "{outcome:?}");
    }

    /// A tool that floods stderr costs this process the tail it keeps,
    /// not the flood: 32 MiB written, the last 4 KiB kept, and the
    /// process's high-water mark where it was.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stderr_is_kept_by_its_tail_alone() {
        fn high_water_mark() -> u64 {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
            let kib: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            kib << 10
        }
        let config = Config {
            timeout: Duration::from_secs(10),
            ..config()
        };
        let before = high_water_mark();
        let flood = sh("head -c 33554432 /dev/zero | tr '\\0' a >&2; echo end >&2; exit 3");
        match run(flood, &config).await.unwrap() {
            Outcome::Failed { code, stderr, .. } => {
                assert_eq!(code, Some(3));
                // The tail, less the newline `tail` trims.
                assert_eq!(stderr.len(), STDERR_KEEP - 1);
                assert!(
                    stderr.ends_with("aaaend"),
                    "{}",
                    &stderr[stderr.len() - 16..]
                );
            }
            other => panic!("{other:?}"),
        }
        let grown = high_water_mark().saturating_sub(before);
        assert!(
            grown < ByteSize::mib(8).as_u64(),
            "the high-water mark grew by {grown} bytes"
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

    /// Isolation `off` is the limits and the timeout, bubblewrap or not.
    #[tokio::test]
    async fn isolation_off_ignores_bubblewrap() {
        let config = Config {
            isolation: Isolation::Off,
            bubblewrap: Some("/usr/bin/bwrap".into()),
            ..config()
        };
        let status = probe(&config).await.unwrap();
        assert_eq!(status.to_string(), "rlimits only (isolation off)");
        let mut t = Command::new("true");
        t.writable(std::env::temp_dir());
        assert!(matches!(run(t, &config).await.unwrap(), Outcome::Success));
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
        let wrapped = Config {
            bubblewrap: Some("/usr/bin/bwrap".into()),
            ..config()
        };
        let err = probe(&wrapped).await.unwrap_err();
        assert!(err.to_string().contains("Linux only"), "{err}");
        let mut t = Command::new("true");
        t.writable(std::env::temp_dir());
        let err = run(t, &wrapped).await.unwrap_err();
        assert!(err.to_string().contains("Linux only"), "{err}");
    }

    /// The configured isolation wins over a required check on another
    /// platform: `auto` and `off` run.
    #[tokio::test]
    async fn the_boot_line_names_every_layer() {
        let status = Status {
            landlock: Some(4),
            seccomp: true,
            bubblewrap: None,
            rlimits_only: None,
        };
        assert_eq!(status.to_string(), "landlock abi 4, seccomp, rlimits");
        assert!(status.confines_filesystem());
        let status = Status {
            landlock: None,
            seccomp: true,
            bubblewrap: None,
            rlimits_only: None,
        };
        assert_eq!(status.to_string(), "no landlock, seccomp, rlimits");
        assert!(!status.confines_filesystem());
        let status = Status {
            landlock: None,
            seccomp: true,
            bubblewrap: Some("0.11.1".to_string()),
            rlimits_only: None,
        };
        assert_eq!(status.to_string(), "bubblewrap 0.11.1, seccomp, rlimits");
        assert!(status.confines_filesystem());
    }

    /// Under bubblewrap Landlock yields to the mount namespace and every
    /// failure is an error; `off` turns bubblewrap off with the rest.
    #[test]
    fn bubblewrap_replaces_landlock_and_is_strict() {
        let wrapped = Config {
            bubblewrap: Some("/usr/bin/bwrap".into()),
            ..config()
        };
        let layers = Layers::of(&wrapped);
        assert!(layers.bubblewrap && layers.seccomp && layers.strict && !layers.landlock);
        let layers = Layers::of(&config());
        assert!(layers.landlock && layers.seccomp && !layers.bubblewrap && !layers.strict);
        let off = Config {
            isolation: Isolation::Off,
            ..wrapped
        };
        let layers = Layers::of(&off);
        assert!(!layers.landlock && !layers.seccomp && !layers.bubblewrap && !layers.strict);
    }

    /// A `bwrap` failure is told from the program's by its stderr, and a
    /// program `bwrap` could not execute is a spawn failure as without it.
    #[test]
    fn bubblewrap_failures_are_read_from_stderr() {
        let program = || "/usr/bin/vips".to_string();
        match bubblewrap_failure(
            program(),
            "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted",
        ) {
            Some(Error::Bubblewrap { message, .. }) => {
                assert!(message.starts_with("bwrap: loopback"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        match bubblewrap_failure(
            program(),
            "bwrap: execvp /usr/bin/vips: No such file or directory",
        ) {
            Some(Error::Spawn { source, .. }) => {
                assert_eq!(source.to_string(), "No such file or directory");
            }
            other => panic!("{other:?}"),
        }
        assert!(bubblewrap_failure(program(), "vips: unable to load").is_none());
        assert!(bubblewrap_failure(program(), "warning\nbwrap: late").is_none());
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

        /// `media.sandbox.readable`, the operator's paths, are readable
        /// by every command and read-only, like a declared path.
        #[tokio::test]
        async fn the_operator_paths_are_readable_and_read_only() {
            let Some(config) = confined().await else {
                return;
            };
            let fonts = tempdir();
            std::fs::write(fonts.path().join("font"), b"glyphs").unwrap();
            let config = Config {
                readable: vec![fonts.path().to_path_buf()],
                ..config
            };
            let work = tempdir();

            let mut cat = Command::new("cat");
            cat.arg(fonts.path().join("font")).writable(work.path());
            assert!(matches!(run(cat, &config).await.unwrap(), Outcome::Success));

            let mut write = sh("echo x > \"$1\"");
            write
                .arg("sh")
                .arg(fonts.path().join("out"))
                .writable(work.path());
            assert!(matches!(
                run(write, &config).await.unwrap(),
                Outcome::Failed { .. }
            ));
            assert!(!fonts.path().join("out").exists());
        }

        /// Under confinement the home and the cache are the writable
        /// directory, so that what a library writes there goes with the
        /// job instead of failing on a home the child cannot write.
        #[tokio::test]
        async fn the_home_is_the_writable_directory() {
            let Some(config) = confined().await else {
                return;
            };
            let work = tempdir();
            let mut home = sh(
                "test \"$HOME\" = \"$1\" && test \"$XDG_CACHE_HOME\" = \"$1\" \
                               && touch \"$HOME/cache\"",
            );
            home.arg("sh").arg(work.path()).writable(work.path());
            let outcome = run(home, &config).await.unwrap();
            assert!(matches!(outcome, Outcome::Success), "{outcome:?}");
            assert!(work.path().join("cache").exists());
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
        pub(super) const SOCKETS: &str = r#"
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

        /// A child cannot leave the process group the timeout kills:
        /// `setsid` and `setpgid` are refused, so a program that tries to
        /// fork something past the timeout stays in the group, and the
        /// timeout finds it. The `sleep` is found by a marker in its
        /// arguments, as in the bubblewrap test.
        #[tokio::test]
        async fn sessions_and_groups_of_their_own_are_refused() {
            let Some(config) = filtered().await else {
                return;
            };
            let work = tempdir();
            if has("python3") {
                let mut calls = Command::new("python3");
                calls
                    .args([
                        "-I",
                        "-c",
                        "import os, sys\n\
                         for call in (os.setsid, lambda: os.setpgid(0, 0)):\n\
                         \ttry:\n\
                         \t\tcall()\n\
                         \t\tsys.exit(10)\n\
                         \texcept PermissionError:\n\
                         \t\tpass\n",
                    ])
                    .writable(work.path());
                let outcome = run(calls, &config).await.unwrap();
                assert!(matches!(outcome, Outcome::Success), "{outcome:?}");
            }
            if !has("setsid") {
                eprintln!("skipped: no setsid");
                return;
            }
            let config = Config {
                timeout: Duration::from_millis(500),
                ..config
            };
            let marker = format!("30.{:06}", std::process::id() % 1_000_000);
            let mut escape = sh("(setsid sleep \"$AGRAFE_SANDBOX_MARKER\" || \
                                 sleep \"$AGRAFE_SANDBOX_MARKER\") & wait");
            escape
                .env("AGRAFE_SANDBOX_MARKER", &marker)
                .writable(work.path());
            assert!(matches!(
                run(escape, &config).await.unwrap(),
                Outcome::TimedOut
            ));
            for _ in 0..200 {
                if !super::bubblewrap::running(&marker) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the forked `sleep {marker}` outlived the timeout");
        }

        /// The worker's priority, affinity and I/O class are out of the
        /// child's reach, as its limits are, and a pid descriptor on it
        /// cannot be opened; the child's own, by pid 0, still work.
        #[tokio::test]
        async fn the_scheduling_of_other_processes_is_refused() {
            let Some(config) = filtered().await else {
                return;
            };
            let work = tempdir();
            for (tool, script) in [
                ("renice", "renice -n 5 -p $PPID"),
                ("taskset", "taskset -cp 0 $PPID"),
                ("ionice", "ionice -c 3 -p $PPID"),
            ] {
                if !has(tool) {
                    eprintln!("skipped: no {tool}");
                    continue;
                }
                let mut other = sh(script);
                other.writable(work.path());
                match run(other, &config).await.unwrap() {
                    Outcome::Failed { stderr, .. } => {
                        assert!(stderr.contains("not permitted"), "{script}: {stderr}");
                    }
                    other => panic!("{script}: {other:?}"),
                }
            }
            if !has("python3") {
                eprintln!("skipped: no python3");
                return;
            }
            let mut own = Command::new("python3");
            own.args([
                "-I",
                "-c",
                "import os, sys\n\
                 os.nice(1)\n\
                 os.setpriority(os.PRIO_PROCESS, 0, 3)\n\
                 os.sched_setaffinity(0, os.sched_getaffinity(0))\n\
                 try:\n\
                 \tos.pidfd_open(int(sys.argv[1]))\n\
                 \tsys.exit(10)\n\
                 except PermissionError:\n\
                 \tpass\n",
            ])
            .arg(std::process::id().to_string())
            .writable(work.path());
            let outcome = run(own, &config).await.unwrap();
            assert!(matches!(outcome, Outcome::Success), "{outcome:?}");
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

    /// The same refusals through bubblewrap, and what its namespaces add:
    /// the configuration file does not exist, the network is loopback
    /// alone, the worker is not in the pid namespace. They need the
    /// packaged `/usr/bin/bwrap`, which is what Ubuntu's AppArmor profile
    /// grants a user namespace to; elsewhere they skip with a message,
    /// unless `AGRAFE_SANDBOX_REQUIRED=1`.
    #[cfg(target_os = "linux")]
    mod bubblewrap {
        use super::*;

        const BWRAP: &str = "/usr/bin/bwrap";

        fn required() -> bool {
            std::env::var_os("AGRAFE_SANDBOX_REQUIRED").is_some_and(|v| v == "1")
        }

        /// The configuration with bubblewrap, probed, or `None` with a
        /// message where the host cannot run it and the tests are not
        /// required.
        async fn wrapped() -> Option<Config> {
            let config = Config {
                bubblewrap: Some(BWRAP.into()),
                ..config()
            };
            if !Path::new(BWRAP).exists() {
                assert!(
                    !required(),
                    "AGRAFE_SANDBOX_REQUIRED=1 but there is no {BWRAP}"
                );
                eprintln!("skipped: no {BWRAP}");
                return None;
            }
            match probe(&config).await {
                Ok(status) => {
                    assert!(status.bubblewrap.is_some() && status.seccomp, "{status}");
                    Some(config)
                }
                Err(err) => {
                    assert!(!required(), "AGRAFE_SANDBOX_REQUIRED=1 but {err}");
                    eprintln!("skipped: {err}");
                    None
                }
            }
        }

        fn has(program: &str) -> bool {
            Command::new(program).program().is_absolute()
        }

        #[tokio::test]
        async fn the_boot_line_names_bubblewrap() {
            let Some(config) = wrapped().await else {
                return;
            };
            let status = probe(&config).await.unwrap();
            assert!(status.landlock.is_none(), "{status}");
            let line = status.to_string();
            let version = line.strip_prefix("bubblewrap ").unwrap_or_default();
            assert!(version.starts_with(|c: char| c.is_ascii_digit()), "{line}");
            assert!(line.ends_with(", seccomp, rlimits"), "{line}");
            let required = Config {
                isolation: Isolation::Required,
                ..config
            };
            assert_eq!(probe(&required).await.unwrap(), status);
        }

        /// What was denied under Landlock does not exist under bubblewrap:
        /// a configuration file next to the binary, `/etc/passwd`, another
        /// job's directory.
        #[tokio::test]
        async fn files_outside_do_not_exist() {
            let Some(config) = wrapped().await else {
                return;
            };
            let beside = tempdir();
            std::fs::write(beside.path().join("agrafe.toml"), b"[keys]").unwrap();
            let work = tempdir();
            std::fs::write(work.path().join("input"), b"input").unwrap();

            let mut cat = Command::new("cat");
            cat.arg(beside.path().join("agrafe.toml"))
                .writable(work.path());
            match run(cat, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("No such file or directory"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
            let mut ls = sh("ls \"$1\" || test -e /etc/passwd || test -e /etc/hostname");
            ls.arg("sh").arg(beside.path()).writable(work.path());
            assert!(matches!(
                run(ls, &config).await.unwrap(),
                Outcome::Failed { .. }
            ));

            let mut cat = Command::new("cat");
            cat.arg(work.path().join("input")).writable(work.path());
            assert!(matches!(run(cat, &config).await.unwrap(), Outcome::Success));
        }

        #[tokio::test]
        async fn writes_outside_are_refused() {
            let Some(config) = wrapped().await else {
                return;
            };
            let work = tempdir();
            let elsewhere = tempdir();

            // Not there, read-only where bound, and the root remounted.
            for target in [
                elsewhere.path().join("out"),
                PathBuf::from("/usr/agrafe-out"),
                PathBuf::from("/agrafe-out"),
            ] {
                let mut write = sh("echo x > \"$1\"");
                write.arg("sh").arg(&target).writable(work.path());
                assert!(
                    matches!(run(write, &config).await.unwrap(), Outcome::Failed { .. }),
                    "{}",
                    target.display()
                );
                assert!(!target.exists());
            }
            let mut write = sh("echo x > \"$1\"");
            write
                .arg("sh")
                .arg(elsewhere.path().join("out"))
                .readable(elsewhere.path())
                .writable(work.path());
            match run(write, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("Read-only file system"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }

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
        }

        /// The operator's paths are bound read-only, and the home is the
        /// writable directory, as under Landlock.
        #[tokio::test]
        async fn the_operator_paths_and_the_home_are_as_under_landlock() {
            let Some(config) = wrapped().await else {
                return;
            };
            let fonts = tempdir();
            std::fs::write(fonts.path().join("font"), b"glyphs").unwrap();
            let config = Config {
                readable: vec![fonts.path().to_path_buf()],
                ..config
            };
            let work = tempdir();

            let mut cat = Command::new("cat");
            cat.arg(fonts.path().join("font")).writable(work.path());
            assert!(matches!(run(cat, &config).await.unwrap(), Outcome::Success));
            let mut write = sh("echo x > \"$1\"");
            write
                .arg("sh")
                .arg(fonts.path().join("out"))
                .writable(work.path());
            match run(write, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("Read-only file system"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
            assert!(!fonts.path().join("out").exists());

            let mut home = sh(
                "test \"$HOME\" = \"$1\" && test \"$XDG_CACHE_HOME\" = \"$1\" \
                               && touch \"$HOME/cache\"",
            );
            home.arg("sh").arg(work.path()).writable(work.path());
            let outcome = run(home, &config).await.unwrap();
            assert!(matches!(outcome, Outcome::Success), "{outcome:?}");
            assert!(work.path().join("cache").exists());
        }

        /// The network namespace has loopback alone, with nothing
        /// listening: seen without the seccomp filter, which refuses the
        /// socket before the namespace matters, and with it.
        #[tokio::test]
        async fn the_network_is_loopback_alone() {
            let Some(config) = wrapped().await else {
                return;
            };
            if !has("python3") {
                eprintln!("skipped: no python3");
                return;
            }
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let work = tempdir();
            // Listing the interfaces takes a socket too, so the filtered
            // run only connects.
            const CONNECT: &str = "\
import socket, sys
try:
    socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=2)
    sys.exit(10)
except ConnectionRefusedError:
    sys.exit(3)
except PermissionError:
    sys.exit(4)
";
            let interfaces = format!(
                "import socket, sys\n\
                 sys.stderr.write(repr([name for _, name in socket.if_nameindex()]) + '\\n')\n\
                 {CONNECT}"
            );
            let connect = |script: &str| {
                let mut python = Command::new("python3");
                python
                    .args(["-I", "-c", script])
                    .arg(port.to_string())
                    .writable(work.path());
                python
            };
            let unfiltered = Layers {
                landlock: false,
                seccomp: false,
                bubblewrap: true,
                strict: true,
            };
            let (outcome, _) = run_with(connect(&interfaces), &config, unfiltered)
                .await
                .unwrap();
            match outcome {
                Outcome::Failed { code, stderr, .. } => {
                    assert_eq!(code, Some(3), "{stderr}");
                    assert!(stderr.starts_with("['lo']"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
            match run(connect(CONNECT), &config).await.unwrap() {
                Outcome::Failed { code, stderr, .. } => {
                    assert_eq!(code, Some(4), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(
                listener.accept().map(|_| ()).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }

        /// The child is pid 2 under `bwrap`, and the worker's pid does
        /// not exist in its namespace.
        #[tokio::test]
        async fn the_worker_is_not_in_the_pid_namespace() {
            let Some(config) = wrapped().await else {
                return;
            };
            let work = tempdir();
            // `/proc` is the namespace's own, without the worker.
            let mut kill = sh("test \"$PPID\" = 1 && test ! -e \"/proc/$1\" && kill -0 \"$1\"");
            kill.arg("sh")
                .arg(std::process::id().to_string())
                .writable(work.path());
            match run(kill, &config).await.unwrap() {
                Outcome::Failed { stderr, .. } => {
                    assert!(stderr.contains("No such process"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
        }

        #[tokio::test]
        async fn namespaces_are_refused() {
            let Some(config) = wrapped().await else {
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

        /// The filters `bwrap` installs are the same: threads work, and a
        /// socket of any family is refused.
        #[tokio::test]
        async fn the_filters_are_installed() {
            let Some(config) = wrapped().await else {
                return;
            };
            let work = tempdir();
            let mut threads = sh("seq 1 200000 | sort --parallel=4 -S 64M > /dev/null && \
                                  seq 1 4 | xargs -P 4 -I{} true");
            threads.writable(work.path());
            match run(threads, &config).await.unwrap() {
                Outcome::Success => {}
                other => panic!("{other:?}"),
            }
            if !has("python3") {
                eprintln!("skipped: no python3");
                return;
            }
            let mut sockets = Command::new("python3");
            sockets
                .args(["-I", "-c", super::refusals::SOCKETS])
                .writable(work.path());
            match run(sockets, &config).await.unwrap() {
                Outcome::Failed { code, stderr, .. } => {
                    assert_ne!(code, Some(10), "an inet socket was allowed");
                    assert!(stderr.contains("PermissionError"), "{stderr}");
                }
                other => panic!("{other:?}"),
            }
        }

        /// LibreOffice's pipe lands in the sandbox's own `/tmp`, which
        /// goes with it: the host's is untouched.
        #[tokio::test]
        async fn unix_sockets_bind_in_the_private_tmp() {
            let Some(config) = wrapped().await else {
                return;
            };
            if !has("python3") {
                eprintln!("skipped: no python3");
                return;
            }
            let work = tempdir();
            let pipe = format!("/tmp/agrafe-sandbox-test-pipe-{}", std::process::id());
            let script = format!(
                "{}\n\
                 s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n\
                 s.bind(sys.argv[1])\n\
                 s.listen(1)\n",
                super::refusals::SOCKETS
            );
            let mut sockets = Command::new("python3");
            sockets
                .args(["-I", "-c", &script])
                .arg(&pipe)
                .writable(work.path())
                .unix_sockets("/tmp");
            match run(sockets, &config).await.unwrap() {
                Outcome::Success => {}
                other => panic!("{other:?}"),
            }
            assert!(
                !Path::new(&pipe).exists(),
                "the pipe reached the host's /tmp"
            );
        }

        /// Whether a process on the host carries `marker` in its
        /// arguments: how a `sleep` forked inside the sandbox is found
        /// from outside, since its pid says nothing out here under a pid
        /// namespace.
        pub(super) fn running(marker: &str) -> bool {
            std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
                std::fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| {
                    cmdline
                        .split(|byte| *byte == 0)
                        .any(|arg| arg == marker.as_bytes())
                })
            })
        }

        /// A timeout takes the whole namespace down. The group the worker
        /// kills holds `bwrap` alone, since its pid 1 starts a session of
        /// its own, so what ends a `sleep` the program forked is
        /// `--die-with-parent`: pid 1 dies with `bwrap`, and the kernel
        /// kills the rest, a `sleep` that asked for a session of its own
        /// included. The `sleep` is found from the host by a marker in
        /// its arguments, since its pid inside the namespace says nothing
        /// out here.
        #[tokio::test]
        async fn timeouts_take_the_namespace_down() {
            let Some(config) = wrapped().await else {
                return;
            };
            let work = tempdir();
            let started = work.path().join("started");
            let marker = format!("30.{:06}", std::process::id() % 1_000_000);
            let mut command = Command::new("sh");
            command
                .args([
                    "-c",
                    "(setsid sleep \"$AGRAFE_SANDBOX_MARKER\" || \
                     sleep \"$AGRAFE_SANDBOX_MARKER\") & echo > \"$1\"; wait",
                    "sh",
                ])
                .arg(&started)
                .env("AGRAFE_SANDBOX_MARKER", &marker)
                .writable(work.path());
            assert!(matches!(
                run(command, &config).await.unwrap(),
                Outcome::TimedOut
            ));
            assert!(started.exists(), "the sleep was never forked");
            // Only the `sleep` carries the marker in its arguments; the
            // shell and `bwrap` had it in the environment alone.
            for _ in 0..200 {
                if !running(&marker) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the forked `sleep {marker}` outlived the timeout");
        }

        /// `bwrap` reports a child the kernel killed as an exit of 128
        /// plus the signal: the CPU limit is still a timeout.
        #[tokio::test]
        async fn signals_read_as_exit_codes() {
            let Some(config) = wrapped().await else {
                return;
            };
            let work = tempdir();
            let mut xcpu = sh("kill -XCPU $$");
            xcpu.writable(work.path());
            assert!(matches!(
                run(xcpu, &config).await.unwrap(),
                Outcome::TimedOut
            ));
            let mut killed = sh("kill -9 $$");
            killed.writable(work.path());
            match run(killed, &config).await.unwrap() {
                Outcome::Failed { code, .. } => assert_eq!(code, Some(128 + libc::SIGKILL)),
                other => panic!("{other:?}"),
            }
        }

        /// A `bwrap` failure is an error, not an outcome of the file's: a
        /// writable directory that does not exist; and a program that
        /// does not exist is the spawn failure it is without bubblewrap.
        #[tokio::test]
        async fn bwrap_failures_are_errors() {
            let Some(config) = wrapped().await else {
                return;
            };
            let mut t = Command::new("true");
            t.writable("/nonexistent/agrafe-work");
            match run(t, &config).await {
                Err(Error::Bubblewrap { message, .. }) => {
                    assert!(message.contains("Can't find source path"), "{message}");
                }
                other => panic!("{other:?}"),
            }
            let missing = Command::new("/nonexistent/agrafe-no-such-program");
            assert!(matches!(
                run(missing, &config).await,
                Err(Error::Spawn { .. })
            ));
        }

        /// A `bwrap` that is not the profiled one gets no user namespace
        /// on a host that restricts them, and the probe says so.
        #[tokio::test]
        async fn an_unprofiled_bwrap_is_a_configuration_failure() {
            let Some(config) = wrapped().await else {
                return;
            };
            let restricted =
                std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                    .is_ok_and(|v| v.trim() == "1");
            if !restricted {
                eprintln!("skipped: this host does not restrict unprivileged user namespaces");
                return;
            }
            let dir = tempdir();
            let copy = dir.path().join("bwrap");
            std::fs::copy(BWRAP, &copy).unwrap();
            let unprofiled = Config {
                bubblewrap: Some(copy.clone()),
                ..config
            };
            match probe(&unprofiled).await {
                Err(ProbeError::Bubblewrap { program, reason }) => {
                    assert_eq!(program, copy.to_string_lossy());
                    assert!(reason.starts_with("bwrap: "), "{reason}");
                }
                other => panic!("{other:?}"),
            }
            let mut t = Command::new("true");
            t.writable(dir.path());
            assert!(matches!(
                run(t, &unprofiled).await,
                Err(Error::Bubblewrap { .. })
            ));
        }

        #[tokio::test]
        async fn spawns_stay_cheap() {
            let Some(config) = wrapped().await else {
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
