//! The `agrafe` binary itself: its configuration, `migrate`, and the exit
//! status of `audit`, which cron and alerting go by.

use std::path::Path;
use std::process::{Command, Output};

use agrafe::db::test::TestDatabase;

/// A configuration on `db`, storing under `storage`, whose rows and
/// objects are settled at once.
fn write_config(dir: &Path, db: &TestDatabase, storage: &Path) -> std::path::PathBuf {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let name = db.connect_options().get_database().unwrap().to_string();
    let sep = if url.contains('?') { '&' } else { '?' };
    let key = "KioqKioqKioqKioqKioqKioqKioqKioqKioqKioqKio=";
    let kek = "KysrKysrKysrKysrKysrKysrKysrKysrKysrKysrKys=";
    let toml = format!(
        r#"
        [server]
        private_addr = "127.0.0.1:0"
        public_addr = "127.0.0.1:0"
        public_base_url = "https://agrafe.test"
        [database]
        url = "{url}{sep}dbname={name}"
        max_connections = 2
        [storage]
        backend = "local"
        [storage.local]
        path = "{storage}"
        [upload]
        stale_after = "0s"
        [clamav]
        addr = "127.0.0.1:1"
        [signing]
        current_key = "k1"
        [signing.keys]
        k1 = "{key}"
        [encryption]
        current_kek = "kek1"
        [encryption.keks]
        kek1 = "{kek}"
        "#,
        storage = storage.display(),
    );
    let path = dir.join("agrafe.toml");
    std::fs::write(&path, toml).unwrap();
    path
}

fn agrafe(config: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agrafe"));
    command.arg("--config").arg(config).args(args);
    // The configuration is the file's alone: `AGRAFE_` variables in the
    // environment would override it.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("AGRAFE_") {
            command.env_remove(name);
        }
    }
    command.env("RUST_LOG", "warn").output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
async fn migrate_then_audit_exits_by_what_it_found() {
    let db = TestDatabase::empty().await;
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("objects");
    std::fs::create_dir(&storage).unwrap();
    let config = write_config(dir.path(), &db, &storage);

    let out = agrafe(&config, &["migrate"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let tables = sqlx_count(&db).await;
    assert!(tables > 0, "the migrations ran");

    // An empty bucket against an empty table: nothing to say.
    let out = agrafe(&config, &["audit"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("objects listed:          0"), "{stdout}");

    // A foreign object is reported and does not fail the run.
    std::fs::write(storage.join("README"), b"not ours").unwrap();
    let out = agrafe(&config, &["audit"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("foreign objects:         1"),
        "{}",
        text(&out.stdout)
    );

    // An object under a prefix no row has is a mismatch: a non-zero exit,
    // the tally still printed.
    let prefix = storage.join("0190b5a2-1c3d-7e4f-8a9b-0c1d2e3f4a5b");
    std::fs::create_dir(&prefix).unwrap();
    std::fs::write(prefix.join("original"), b"lost").unwrap();
    let out = agrafe(&config, &["audit"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("objects with no row:     1"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stderr).contains("the audit found 1 mismatches"),
        "{}",
        text(&out.stderr)
    );

    // `--delete-unknown` deletes it: what is fixed is still reported, and
    // the next run is clean.
    let out = agrafe(&config, &["audit", "--delete-unknown"]);
    assert!(text(&out.stdout).contains("objects deleted:         1"));
    assert!(!prefix.join("original").exists());
    let out = agrafe(&config, &["audit"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn a_missing_config_file_or_a_bad_flag_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = agrafe(&dir.path().join("nope.toml"), &["migrate"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("nope.toml not found"),
        "{}",
        text(&out.stderr)
    );

    // `--sample` means nothing without `--deep`.
    let out = agrafe(&dir.path().join("nope.toml"), &["audit", "--sample", "2"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
}

#[test]
fn version_is_the_workspace_version() {
    let dir = tempfile::tempdir().unwrap();
    // Needs no configuration: the file is not read.
    let out = agrafe(&dir.path().join("nope.toml"), &["--version"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        concat!("agrafe ", env!("CARGO_PKG_VERSION"), "\n")
    );
}

async fn sqlx_count(db: &TestDatabase) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM information_schema.tables WHERE table_name = 'blobs'")
        .fetch_one(db.pool())
        .await
        .unwrap()
}
