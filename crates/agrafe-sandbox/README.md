# agrafe-sandbox

[![crates.io](https://img.shields.io/crates/v/agrafe-sandbox.svg)](https://crates.io/crates/agrafe-sandbox)
[![docs.rs](https://img.shields.io/docsrs/agrafe-sandbox)](https://docs.rs/agrafe-sandbox)

Runs decoders and renderers as confined subprocesses, for
[agrafe](https://github.com/tchak/agrafe), the file storage service: the
code that turns untrusted bytes into pixels never runs in the server, so a
crash, a decoder bomb or an infinite loop costs one job attempt, not the
process. Three layers:

- `setrlimit` bounds on the child and a wall-clock timeout, on every
  platform, the child's process group killed whole at the timeout;
- a Landlock ruleset, on Linux: the system set and the paths the `Command`
  declares are readable, its one work directory writable, TCP and signals
  outside the sandbox refused, nothing else there;
- a seccomp denylist, on Linux: no sockets (unix sockets alone on
  request), no namespaces, no ptrace, no mounts, and no other
  kernel-facing surface a renderer has any use for.

Where the host grants unprivileged user namespaces, bubblewrap can put
mount, pid and network namespaces around the same child. `Isolation::Auto`
confines where the kernel can, `Isolation::Required` refuses to start
without it, and `probe` reports what a process got. See
[Sandboxed rendering](https://tchak.github.io/agrafe/guide/security.html#sandboxed-rendering)
and the [sandbox](https://tchak.github.io/agrafe/guide/deployment.html#the-sandbox)
section of the deployment guide.

## Part of agrafe

A building block of [agrafe](https://github.com/tchak/agrafe), the file
storage service. It can be used on its own, but its API serves agrafe's
and moves with its releases: every agrafe crate is released together, at
one version, and before 1.0 a minor version may break it, as the
[changelog](https://github.com/tchak/agrafe/blob/main/CHANGELOG.md) says.
To run agrafe, see [agrafe-server](https://crates.io/crates/agrafe-server)
and the [documentation](https://tchak.github.io/agrafe/).

## License

Licensed under either of the [Apache License, Version 2.0](https://github.com/tchak/agrafe/blob/main/LICENSE-APACHE)
or the [MIT license](https://github.com/tchak/agrafe/blob/main/LICENSE-MIT),
at your option.
