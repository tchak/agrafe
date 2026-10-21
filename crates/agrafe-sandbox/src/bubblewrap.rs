//! bubblewrap around the child: the `bwrap` argument list built from the
//! same system set and declarations as the Landlock ruleset, and the
//! seccomp filters handed over as inherited descriptors.
//!
//! What it adds over Landlock: a mount namespace where the configuration
//! file does not exist rather than is denied, a pid namespace where the
//! worker does not exist, a network namespace with loopback alone, a
//! fresh bounded `/tmp`, and a uid of the child's own. The filters are
//! the ones `confine` compiles; `bwrap` installs them in the child after
//! the namespaces are set up, since it needs `unshare`, `mount` and
//! `pivot_root` itself, which the filter refuses. `--remount-ro /` comes
//! last: nothing mounts after it.

use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::{self, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use seccompiler::BpfProgram;

use crate::Command;
use crate::confine::{NULL, SYSTEM};

/// The uid and gid the child has inside its user namespace, mapped from
/// the worker's: the files the worker owns are its, and nothing else is.
const ID: &str = "10000";
/// What the child sees as its hostname.
const HOSTNAME: &str = "agrafe";
/// The fresh `/tmp`, a tmpfs bounded to this size. Nothing agrafe runs
/// writes there but LibreOffice's pipe: the work directory, under the
/// host's `/tmp` by default, is bound back in at its own path.
const TMP: &str = "/tmp";
const TMPFS_SIZE: u64 = 64 << 20;
/// A procfs of the pid namespace, which shows the sandbox's own
/// processes and nothing of the worker's: LibreOffice refuses to start
/// without one. Landlock grants no `/proc`, since the worker's own
/// `/proc/<pid>` would be in it.
const PROC: &str = "/proc";
/// The prefix of `bwrap --version`'s answer.
const VERSION_PREFIX: &str = "bubblewrap ";

/// A descriptor the child inherits: close-on-exec in this process, until
/// the child clears it between `fork` and `exec`.
pub(crate) struct Descriptor(File);

impl Descriptor {
    /// A memory file holding `bytes`, read from the start.
    fn new(name: &CStr, bytes: &[u8]) -> io::Result<Self> {
        // SAFETY: `memfd_create` reads a NUL-terminated name and returns
        // a new descriptor, owned by the `File` from here on.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor is open and owned by nothing else.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes)?;
        file.rewind()?;
        Ok(Self(file))
    }

    /// The number `bwrap` is told.
    fn number(&self) -> i32 {
        self.0.as_raw_fd()
    }

    /// Let the descriptor survive `exec`: one `fcntl`, fit for the child
    /// between `fork` and `exec`.
    pub(crate) fn inherit(&self) -> io::Result<()> {
        // SAFETY: `fcntl` on a descriptor this process owns, with a flag
        // argument.
        if unsafe { libc::fcntl(self.number(), libc::F_SETFD, 0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// `bwrap`'s arguments for a command, and the descriptors its child must
/// inherit.
pub(crate) struct Wrapped {
    pub(crate) args: Vec<OsString>,
    pub(crate) descriptors: Vec<Descriptor>,
}

/// The `bwrap` argument list for `command`: the namespaces, the fresh
/// `/tmp` and `/proc`, the system set and the program read-only, the extra and
/// declared paths read-only, the writable directory read-write at its
/// own path, the filters, then the program and its arguments. Paths
/// missing on the host are skipped, as the ruleset skips them; the
/// writable directory is not, since a caller that declares one that does
/// not exist has a bug, which `bwrap` then reports. Nothing is bound for
/// the unix socket directory: inside the sandbox `/tmp` is a writable
/// tmpfs, which is where a program that needs a socket puts it (LibreOffice
/// checks `/tmp` first, and finds it writable), and a read-write bind of
/// the host's directory would grant far more than the ruleset's right to
/// make and remove sockets there.
pub(crate) fn wrap(
    command: &Command,
    extra: &[PathBuf],
    filters: Option<&[BpfProgram]>,
) -> io::Result<Wrapped> {
    let mut args: Vec<OsString> = [
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--new-session",
        "--die-with-parent",
        "--cap-drop",
        "ALL",
        "--uid",
        ID,
        "--gid",
        ID,
        "--hostname",
        HOSTNAME,
        "--size",
        &TMPFS_SIZE.to_string(),
        "--tmpfs",
        TMP,
        "--proc",
        PROC,
        "--chdir",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(
        command
            .current_dir
            .as_deref()
            .unwrap_or(Path::new("/"))
            .into(),
    );
    let mut mount = |option: &str, path: &Path| {
        args.push(option.into());
        args.push(path.into());
        args.push(path.into());
    };
    for path in SYSTEM.iter().map(Path::new) {
        if !path.exists() {
            continue;
        }
        let option = if path.starts_with("/dev") {
            "--dev-bind"
        } else {
            "--ro-bind"
        };
        mount(option, path);
    }
    if Path::new(NULL).exists() {
        mount("--dev-bind", Path::new(NULL));
    }
    for path in std::iter::once(&command.program)
        .chain(extra)
        .chain(&command.readable)
    {
        if path.exists() {
            mount("--ro-bind", path);
        }
    }
    if let Some(dir) = &command.writable {
        mount("--bind", dir);
    }
    let mut descriptors = Vec::new();
    for filter in filters.unwrap_or_default() {
        let descriptor = Descriptor::new(c"agrafe-seccomp", &bytes(filter))?;
        args.push("--add-seccomp-fd".into());
        args.push(descriptor.number().to_string().into());
        descriptors.push(descriptor);
    }
    args.extend(["--remount-ro", "/", "--"].map(OsString::from));
    args.push(command.program.clone().into());
    args.extend(command.args.iter().cloned());
    Ok(Wrapped { args, descriptors })
}

/// The filter as the kernel takes it, which is how `bwrap` reads it: the
/// `sock_filter` instructions back to back, in native byte order.
fn bytes(program: &BpfProgram) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(program.len() * 8);
    for instruction in program {
        bytes.extend_from_slice(&instruction.code.to_ne_bytes());
        bytes.push(instruction.jt);
        bytes.push(instruction.jf);
        bytes.extend_from_slice(&instruction.k.to_ne_bytes());
    }
    bytes
}

/// The version `bwrap --version` reports, for the boot line, or why it
/// could not be asked.
pub(crate) async fn version(bwrap: &Path, timeout: Duration) -> Result<String, String> {
    let mut command = tokio::process::Command::new(bwrap);
    command
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("`{} --version` did not answer", bwrap.display()))?
        .map_err(|err| format!("running `{} --version`: {err}", bwrap.display()))?;
    if !output.status.success() {
        return Err(format!(
            "`{} --version` failed ({}): {}",
            bwrap.display(),
            output.status,
            crate::tail(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or_default().trim();
    Ok(line
        .strip_prefix(VERSION_PREFIX)
        .unwrap_or(line)
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_argument_list_follows_the_template() {
        let work = tempfile::tempdir().unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "true"])
            .current_dir(work.path())
            .readable("/nonexistent/agrafe")
            .writable(work.path())
            .unix_sockets("/tmp");
        let extra = [PathBuf::from("/etc/fonts"), PathBuf::from("/nonexistent")];
        let filters = crate::confine::filters(false).unwrap();
        let wrapped = wrap(&command, &extra, Some(&filters)).unwrap();
        let args = strings(&wrapped.args);
        let work = work.path().to_string_lossy().into_owned();

        assert!(args.starts_with(&[
            "--unshare-all".to_string(),
            "--unshare-user".to_string(),
            "--disable-userns".to_string(),
        ]));
        let position = |what: &str| args.iter().position(|arg| arg == what).unwrap();
        assert_eq!(args[position("--tmpfs") + 1], "/tmp");
        assert_eq!(args[position("--proc") + 1], "/proc");
        assert_eq!(args[position("--chdir") + 1], work);
        assert_eq!(args[position("--bind") + 1], work);
        assert_eq!(args[position("--bind") + 2], work);
        // The system set and `/dev/null`, device nodes as devices.
        assert_eq!(args[position("/usr") - 1], "--ro-bind");
        assert_eq!(args[position("/dev/null") - 1], "--dev-bind");
        // Missing paths are skipped; the socket directory is not bound.
        assert!(!args.iter().any(|arg| arg.contains("nonexistent")));
        assert_eq!(args.iter().filter(|arg| *arg == "--bind").count(), 1);
        // Two filters, two descriptors.
        assert_eq!(wrapped.descriptors.len(), 2);
        assert_eq!(
            args.iter().filter(|arg| *arg == "--add-seccomp-fd").count(),
            2
        );
        // The remount comes last, then the program.
        let end = position("--remount-ro");
        assert_eq!(&args[end..end + 4], ["--remount-ro", "/", "--", "/bin/sh"]);
        assert_eq!(&args[end + 4..], ["-c", "true"]);
        assert!(args[..end].iter().all(|arg| arg != "--"));
    }

    /// A socket directory outside `/tmp` is not bound either: the
    /// sockets go in the private `/tmp`, and a read-write bind of the
    /// host's `/var/tmp` would be the other jobs' directories.
    #[test]
    fn a_socket_directory_is_never_bound() {
        let pipes = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let mut command = Command::new("/bin/sh");
        command.unix_sockets(pipes.path());
        let wrapped = wrap(&command, &[], None).unwrap();
        let args = strings(&wrapped.args);
        assert!(!args.iter().any(|arg| arg == "--bind"));
        let pipes = pipes.path().to_string_lossy();
        assert!(!args.iter().any(|arg| *arg == pipes));
        assert!(wrapped.descriptors.is_empty());
        assert!(!args.iter().any(|arg| arg == "--add-seccomp-fd"));
    }

    #[test]
    fn filters_serialise_eight_bytes_an_instruction() {
        let filters = crate::confine::filters(false).unwrap();
        for filter in filters.iter() {
            let bytes = bytes(filter);
            assert_eq!(bytes.len(), filter.len() * 8);
            let descriptor = Descriptor::new(c"test", &bytes).unwrap();
            let mut back = Vec::new();
            std::io::Read::read_to_end(&mut &descriptor.0, &mut back).unwrap();
            assert_eq!(back, bytes);
        }
    }
}
