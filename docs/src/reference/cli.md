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
| `--kinds <KIND,...>` | `jobs.kinds` | Comma-separated job kinds this process runs workers for: `fetch`, `scan`, `render`, `rows`, `archive`, `document`, `purge`, `reap`, `notify`, `audit` |

## `agrafe audit`

Checks the bucket against the `blobs` table once, as the weekly `audit`
job does, then exits. One `warn` log line per mismatch, the tally on
standard output, and a non-zero status when anything was found or could
not be acted on; a foreign object, a key that is not agrafe's, is
reported and does not count. See
[Jobs and retries](../guide/jobs.md#the-audit).

```sh
agrafe --config agrafe.toml audit [--fix] [--delete-unknown] [--deep [--sample <N>]]
```

| Option | Description |
|---|---|
| `--fix` | Delete the orphan objects older than `upload.stale_after`: the objects under a known blob that its row does not account for. Without it they are reported and left alone. Never touches a prefix no row has |
| `--delete-unknown` | Delete the objects under a prefix no row has, older than `upload.stale_after`. After a database restored from backup these are the blobs the backup missed, rows to recover rather than objects to delete: not until the database is as recovered as it will be |
| `--deep` | Decrypt the originals and compare their SHA-256 with the rows'. Reads the whole bucket; the listing alone catches a missing or truncated object |
| `--sample <N>` | With `--deep`: read one original in N, picked by id, so the same ones every run |

It needs the database and the bucket, and the KEKs for `--deep`. It runs
on its own connection and holds a lock the job honours: a job starting
meanwhile steps aside, and the command reports a running job and exits.

## Environment

| Variable | Description |
|---|---|
| `AGRAFE_CONFIG` | The configuration file, as `--config` |
| `AGRAFE_<SECTION>__<KEY>` | Any configuration key, see [Configuration](configuration.md) |
| `RUST_LOG` | Log filter, `info` by default, in [`tracing-subscriber`'s syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) |

## Helpers

`agrafe-extract`, `agrafe-render-text`, `agrafe-render-document` and
`agrafe-render-geo` are run by the workers, never by hand. They read no
configuration and write no logs. They must be on the `PATH`, or named by
the `media.*_command` keys.

## Exit

The process exits with a non-zero status and a one-line message when it
cannot start: a missing configuration file, an invalid configuration, an
unreachable database, a failed migration, an address already in use, or a
failed renderer probe. `agrafe audit` also exits non-zero when it found
mismatches (foreign objects aside), when a stamp, a render request or a
delete failed and the finding stands for the next run, or when another
audit was running.
