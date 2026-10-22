//! Landlock and seccomp, prepared in the parent and installed by the child
//! between `fork` and `exec`.
//!
//! The ruleset is built here, where opening paths and allocating are
//! fine; the child only makes the `landlock_restrict_self`, `prctl` and
//! `seccomp` calls. Both layers set
//! `no_new_privs`, so the child cannot regain anything through a setuid
//! binary either.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, RulesetAttr, RulesetCreated, RulesetCreatedAttr,
    RulesetError, RulesetStatus, Scope, path_beneath_rules,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use crate::{Applied, Layers};

/// What a decoder legitimately reads, granted read-only, with execute
/// where it applies: the libraries, the loader's cache, the fonts and
/// their cache, the time zone, the random and zero devices. Paths missing
/// on a host are skipped. Fonts and poppler's data are here on purpose:
/// without them a PDF with non-embedded fonts renders as a blank page and
/// exits 0, which nothing reports.
pub(crate) const SYSTEM: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/etc/ld.so.cache",
    "/etc/alternatives",
    "/etc/fonts",
    "/etc/localtime",
    "/var/cache/fontconfig",
    "/dev/urandom",
    "/dev/zero",
];

/// The one device a tool may also write: a library that silences a
/// stream reopens it on `/dev/null`.
pub(crate) const NULL: &str = "/dev/null";

/// The newest ABI this build knows; the crate degrades to what the kernel
/// has.
const LATEST: ABI = ABI::V9;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("isolation = \"required\" but the kernel has no Landlock")]
    NoLandlock,
    #[error("isolation = \"required\" but the Landlock ruleset cannot be built: {0}")]
    Landlock(RulesetError),
    #[error("isolation = \"required\" but the seccomp filter does not compile: {0}")]
    Seccomp(String),
}

/// What one spawn installs in its child.
pub(crate) struct Plan {
    ruleset: Option<RulesetCreated>,
    filters: Option<Arc<Vec<BpfProgram>>>,
    strict: bool,
}

impl Plan {
    pub(crate) fn new(
        program: &Path,
        readable: &[PathBuf],
        writable: Option<&Path>,
        unix_sockets: Option<&Path>,
        extra: &[PathBuf],
        layers: Layers,
    ) -> Result<Self, Error> {
        let ruleset = if layers.landlock {
            match kernel_abi() {
                None if layers.strict => return Err(Error::NoLandlock),
                None => None,
                Some(_) => match ruleset(program, readable, writable, unix_sockets, extra) {
                    Ok(ruleset) => Some(ruleset),
                    Err(err) if layers.strict => return Err(Error::Landlock(err)),
                    Err(_) => None,
                },
            }
        } else {
            None
        };
        let filters = if layers.seccomp {
            match filters(unix_sockets.is_some()) {
                Ok(filters) => Some(filters),
                Err(err) if layers.strict => return Err(Error::Seccomp(err)),
                Err(_) => None,
            }
        } else {
            None
        };
        Ok(Self {
            ruleset,
            filters,
            strict: layers.strict,
        })
    }

    /// The layers this plan will install, decided in the parent. Whether
    /// the child managed each is known only by what it then refuses: under
    /// strict a failure fails the spawn, otherwise the layer is skipped
    /// without a report, and [`probe`](crate::probe) is what checks, at
    /// startup, that the refusals are real.
    pub(crate) fn applied(&self) -> Applied {
        Applied {
            landlock: self.ruleset.is_some(),
            seccomp: self.filters.is_some(),
        }
    }

    /// Install the layers in the calling process, which is the child
    /// between `fork` and `exec`: nothing here allocates or takes a lock
    /// on the happy path. A layer that fails is skipped unless strict.
    ///
    /// The ruleset is taken, not cloned: a [`Command`](crate::Command) is
    /// spawned once, so the closure runs once, and `restrict_self`
    /// consumes it.
    pub(crate) fn install(&mut self) -> io::Result<()> {
        if let Some(ruleset) = self.ruleset.take() {
            let enforced = match ruleset.restrict_self() {
                Ok(status) => status.ruleset != RulesetStatus::NotEnforced,
                Err(err) if self.strict => return Err(io::Error::from_raw_os_error(errno(&err))),
                Err(_) => false,
            };
            if !enforced && self.strict {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
        }
        if let Some(filters) = &self.filters {
            for filter in filters.iter() {
                if let Err(err) = seccompiler::apply_filter(filter) {
                    if self.strict {
                        let errno = match err {
                            seccompiler::Error::Seccomp(source)
                            | seccompiler::Error::Prctl(source) => {
                                source.raw_os_error().unwrap_or(libc::EPERM)
                            }
                            _ => libc::EPERM,
                        };
                        return Err(io::Error::from_raw_os_error(errno));
                    }
                    break;
                }
            }
        }
        Ok(())
    }
}

/// The errno behind a failed `restrict_self`, `EPERM` when there is none.
/// Walks the source chain without formatting anything, since this runs in
/// the child.
fn errno(err: &RulesetError) -> i32 {
    let mut source = std::error::Error::source(err);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            return io.raw_os_error().unwrap_or(libc::EPERM);
        }
        source = err.source();
    }
    libc::EPERM
}

/// The Landlock ABI the running kernel implements, `None` when it has no
/// Landlock or the LSM is not enabled.
pub(crate) fn kernel_abi() -> Option<i32> {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    // SAFETY: with a null attribute and a zero size, the version flag
    // makes the call a pure query, as the kernel documents.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    i32::try_from(abi).ok().filter(|abi| *abi > 0)
}

/// The ruleset: read and execute on the system set and the program, read
/// on the extra and declared paths, everything but execute on the
/// writable directory, socket files made and removed under the unix
/// socket directory when there is one, no TCP, no signal to a process
/// outside the domain (the child shares the server's uid, so `kill` would
/// reach the worker), no abstract unix socket, and nothing else. Handled
/// accesses the kernel does not know are dropped, which is the
/// best-effort mode.
fn ruleset(
    program: &Path,
    readable: &[PathBuf],
    writable: Option<&Path>,
    unix_sockets: Option<&Path>,
    extra: &[PathBuf],
) -> Result<RulesetCreated, RulesetError> {
    let read = AccessFs::from_read(LATEST);
    let read_only: BitFlags<AccessFs> = read & !AccessFs::Execute;
    let mut write = AccessFs::from_all(LATEST);
    write.remove(AccessFs::Execute);

    let mut ruleset = landlock::Ruleset::default()
        .handle_access(AccessFs::from_all(LATEST))?
        .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?
        .scope(Scope::Signal | Scope::AbstractUnixSocket)?
        .create()?
        .add_rules(path_beneath_rules(SYSTEM, read))?
        .add_rules(path_beneath_rules(
            [NULL],
            AccessFs::ReadFile | AccessFs::WriteFile,
        ))?
        .add_rules(path_beneath_rules([program], read))?
        .add_rules(path_beneath_rules(extra, read_only))?
        .add_rules(path_beneath_rules(readable, read_only))?;
    if let Some(dir) = writable {
        ruleset = ruleset.add_rules(path_beneath_rules([dir], write))?;
    }
    if let Some(dir) = unix_sockets {
        ruleset = ruleset.add_rules(path_beneath_rules(
            [dir],
            AccessFs::MakeSock | AccessFs::RemoveFile,
        ))?;
    }
    Ok(ruleset)
}

#[cfg(target_arch = "x86_64")]
const ARCH: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
const ARCH: TargetArch = TargetArch::aarch64;
#[cfg(target_arch = "riscv64")]
const ARCH: TargetArch = TargetArch::riscv64;

/// Refused outright with `EPERM`: sockets of every family, which also
/// closes path-based unix sockets (Postgres), which no Landlock ABI
/// governs; tracing, cross-process memory and descriptor theft
/// (`pidfd_getfd` reaches the worker's sockets from the same uid when
/// Yama allows it); namespaces, mounts and the new mount API; a session
/// or process group of the child's own, which would take it out of the
/// group the timeout kills; what reaches another process of the same
/// uid by a pid or a pid descriptor (`ioprio_set`, `process_madvise`,
/// which pages the worker's memory out, `migrate_pages`, `move_pages`,
/// and `pidfd_open` itself, which a decoder has no use for); and the
/// kernel-facing surface a decoder has no use for either.
const DENIED: &[libc::c_long] = &[
    libc::SYS_setsid,
    libc::SYS_setpgid,
    libc::SYS_ioprio_set,
    libc::SYS_process_madvise,
    libc::SYS_migrate_pages,
    libc::SYS_move_pages,
    libc::SYS_pidfd_open,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_pidfd_getfd,
    libc::SYS_kcmp,
    libc::SYS_perf_event_open,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_open_tree,
    libc::SYS_move_mount,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_mount_setattr,
    libc::SYS_bpf,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_userfaultfd,
    libc::SYS_io_uring_setup,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
];

/// The socket calls, refused whole by default. With unix sockets allowed,
/// `socket` and `socketpair` are refused for every family but `AF_UNIX`,
/// and the rest, which only act on a socket that exists, are allowed.
const SOCKETS: &[libc::c_long] = &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
];

/// Calls that take a pid and act on the process itself when it is 0,
/// refused for any other: the same uid may set another process's limits,
/// affinity and scheduling, and the worker is one. glibc's `setrlimit`
/// and `getrlimit` pass 0 to `prlimit64`, and a thread pool that pins or
/// schedules itself passes 0 too.
const OWN_PID_ONLY: &[libc::c_long] = &[
    libc::SYS_prlimit64,
    libc::SYS_sched_setaffinity,
    libc::SYS_sched_setscheduler,
    libc::SYS_sched_setparam,
    libc::SYS_sched_setattr,
];

/// The `clone` flags that create a namespace, each refused on its own:
/// a thread or a plain fork carries none of them.
const NAMESPACE_FLAGS: &[u64] = &[
    0x0000_0080, // CLONE_NEWTIME
    0x0002_0000, // CLONE_NEWNS
    0x0200_0000, // CLONE_NEWCGROUP
    0x0400_0000, // CLONE_NEWUTS
    0x0800_0000, // CLONE_NEWIPC
    0x1000_0000, // CLONE_NEWUSER
    0x2000_0000, // CLONE_NEWPID
    0x4000_0000, // CLONE_NEWNET
];

/// The two filters, compiled once per variant: the denylist answering
/// `EPERM`, with `clone` refused under a namespace flag and `prlimit64`
/// refused for any pid but 0 (the same uid may set another process's
/// limits, and the worker is one; glibc's own `setrlimit` and `getrlimit`
/// pass 0), and `clone3` answering `ENOSYS`. `clone3` takes its flags in
/// a struct the filter cannot read, so it is refused whole, and with the
/// one errno glibc falls back from to `clone`, which the first filter does
/// inspect: threads keep working, namespaces do not. The variant with
/// unix sockets refuses every other socket family instead of every
/// socket.
pub(crate) fn filters(unix_sockets: bool) -> Result<Arc<Vec<BpfProgram>>, String> {
    type Compiled = OnceLock<Result<Arc<Vec<BpfProgram>>, String>>;
    static NO_SOCKETS: Compiled = OnceLock::new();
    static UNIX_SOCKETS: Compiled = OnceLock::new();
    let compiled = if unix_sockets {
        &UNIX_SOCKETS
    } else {
        &NO_SOCKETS
    };
    compiled
        .get_or_init(|| {
            compile(unix_sockets)
                .map(Arc::new)
                .map_err(|err| err.to_string())
        })
        .clone()
}

/// A rule that matches, and so refuses, when argument `arg` is not
/// `value`. The arguments here are C `int`s, read as the kernel reads
/// them, in their low 32 bits.
fn unless(arg: u8, value: u64) -> Result<SeccompRule, seccompiler::Error> {
    Ok(SeccompRule::new(vec![SeccompCondition::new(
        arg,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        value,
    )?])?)
}

fn compile(unix_sockets: bool) -> Result<Vec<BpfProgram>, seccompiler::Error> {
    let mut denied: BTreeMap<i64, Vec<SeccompRule>> =
        DENIED.iter().map(|&nr| (nr, Vec::new())).collect();
    if unix_sockets {
        for nr in [libc::SYS_socket, libc::SYS_socketpair] {
            let other_family = SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                libc::AF_UNIX as u64,
            )?])?;
            denied.insert(nr, vec![other_family]);
        }
    } else {
        denied.extend(SOCKETS.iter().map(|&nr| (nr, Vec::new())));
    }
    let namespaces = NAMESPACE_FLAGS
        .iter()
        .map(|&flag| {
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Qword,
                SeccompCmpOp::MaskedEq(flag),
                flag,
            )?])
        })
        .collect::<Result<Vec<_>, _>>()?;
    denied.insert(libc::SYS_clone, namespaces);
    for nr in OWN_PID_ONLY {
        denied.insert(*nr, vec![unless(0, 0)?]);
    }
    // `setpriority(which, who, prio)`: `PRIO_PROCESS` and `who` 0 is the
    // process itself, which is what `nice` does; `PRIO_USER` with 0 would
    // be every process of the uid, the worker included.
    denied.insert(
        libc::SYS_setpriority,
        vec![unless(0, libc::PRIO_PROCESS as u64)?, unless(1, 0)?],
    );
    let eperm = SeccompFilter::new(
        denied,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        ARCH,
    )?;
    let enosys = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        ARCH,
    )?;
    Ok(vec![eperm.try_into()?, enosys.try_into()?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_compile() {
        for unix_sockets in [false, true] {
            let filters = filters(unix_sockets).unwrap();
            assert_eq!(filters.len(), 2);
            assert!(filters.iter().all(|f| !f.is_empty()));
        }
    }

    #[test]
    fn plans_without_layers_apply_nothing() {
        let plan = Plan::new(
            Path::new("/bin/true"),
            &[],
            None,
            None,
            &[],
            Layers {
                landlock: false,
                seccomp: false,
                bubblewrap: false,
                strict: true,
            },
        )
        .unwrap();
        let applied = plan.applied();
        assert!(!applied.landlock && !applied.seccomp);
    }
}
