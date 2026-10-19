# Command line

The server binary is `agrafe`.

```text
agrafe [--config <PATH>] <COMMAND>
```

| Option | Environment | Description |
|---|---|---|
| `--config <PATH>` | `AGRAFE_CONFIG` | The TOML configuration file. Optional: without it, the configuration comes from `AGRAFE_` variables alone. A path that does not exist is an error |

## `agrafe migrate`

Applies the pending database migrations, then exits. Run it once per
release, before starting the new processes.

```sh
agrafe --config agrafe.toml migrate
```

## `agrafe serve`

Runs the HTTP interfaces, the workers, or both, until `SIGINT` or
`SIGTERM`.

```sh
agrafe --config agrafe.toml serve [--role <ROLE>] [--kinds <KIND,...>]
```

| Option | Overrides | Description |
|---|---|---|
| `--role <ROLE>` | `server.role` | `api`, `worker` or `all` |
| `--kinds <KIND,...>` | `jobs.kinds` | Comma-separated job kinds this process runs workers for: `fetch`, `scan`, `render`, `rows`, `archive`, `document`, `purge`, `reap`, `notify` |

## Environment

| Variable | Description |
|---|---|
| `AGRAFE_CONFIG` | The configuration file, as `--config` |
| `AGRAFE_<SECTION>__<KEY>` | Any configuration key, see [Configuration](configuration.md) |
| `RUST_LOG` | Log filter, `info` by default, in [`tracing-subscriber`'s syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) |

## Helpers

`agrafe-extract`, `agrafe-render-text` and `agrafe-render-document` are run
by the workers, never by hand. They read no configuration and write no logs.
They must be on the `PATH`, or named by the `media.*_command` keys.

## Exit

The process exits with a non-zero status and a one-line message when it
cannot start: a missing configuration file, an invalid configuration, an
unreachable database, a failed migration, an address already in use, or a
failed renderer probe.
