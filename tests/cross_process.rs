//! Concurrency across processes: one writing program, other programs reading
//! the same database at the same time.
//!
//! Each scenario runs as a pair of tests. The `child_*` test is the reader
//! program; it does nothing unless `BYTESKLAD_TEST_DB` is set, which only
//! happens when a parent test re-executes this binary with `--exact` to run
//! that one test in a separate process.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use bytesklad::{DbRO, DbRW, DbRead, DbWrite, Options};

const DB_ENV: &str = "BYTESKLAD_TEST_DB";
const TIMEOUT: Duration = Duration::from_secs(60);

fn key(n: u64) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[24..].copy_from_slice(&n.to_be_bytes());
    key
}

fn value(n: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| (n as u8).wrapping_add(i as u8)).collect()
}

/// The database a parent handed to this process, or `None` when running as an
/// ordinary test rather than as a spawned reader.
fn assigned_db() -> Option<PathBuf> {
    env::var_os(DB_ENV).map(PathBuf::from)
}

fn spawn_reader(test: &str, db: &Path) -> std::process::Child {
    Command::new(env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(DB_ENV, db)
        .spawn()
        .unwrap()
}

fn signal(db: &Path, name: &str) {
    fs::write(db.join(name), b"").unwrap();
}

fn await_signal(db: &Path, name: &str) {
    let deadline = Instant::now() + TIMEOUT;
    let path = db.join(name);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for `{name}`");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn reader_process_sees_writer_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path();

    // The writer stays open for the whole test, exactly as a long-running
    // ingest process would.
    let writer = DbRW::<32>::open_or_create(db, &Options::new()).unwrap();
    writer.insert(&key(1), b"first").unwrap();

    let mut reader = spawn_reader("child_sees_writer_commits", db);
    await_signal(db, "reader-ready");

    writer.insert(&key(2), b"second").unwrap();
    signal(db, "go");

    assert!(reader.wait().unwrap().success(), "reader process failed");
}

#[test]
fn child_sees_writer_commits() {
    let Some(db) = assigned_db() else { return };

    let reader = DbRO::<32>::open(&db).unwrap();
    assert_eq!(reader.get(&key(1)).unwrap().unwrap(), b"first");
    assert!(reader.get(&key(2)).unwrap().is_none());

    signal(&db, "reader-ready");
    await_signal(&db, "go");

    // A fresh read transaction picks up what the writer committed after this
    // process opened the database.
    assert_eq!(reader.get(&key(2)).unwrap().unwrap(), b"second");
    assert_eq!(reader.len().unwrap(), 2);
}

#[test]
fn reader_process_follows_a_growing_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path();

    let writer = DbRW::<32>::open_or_create(db, &Options::new().bucket(100)).unwrap();
    writer.insert(&key(0), &value(0, 100)).unwrap();

    let mut reader = spawn_reader("child_follows_a_growing_bucket", db);
    await_signal(db, "reader-ready");

    // The reader has already opened the bucket file by now, so these appends
    // land past the end it saw when it did.
    for n in 1..2_000 {
        writer.insert(&key(n), &value(n, 100)).unwrap();
    }
    signal(db, "go");

    assert!(reader.wait().unwrap().success(), "reader process failed");
}

#[test]
fn child_follows_a_growing_bucket() {
    let Some(db) = assigned_db() else { return };

    let reader = DbRO::<32>::open(&db).unwrap();
    assert_eq!(reader.get(&key(0)).unwrap().unwrap(), value(0, 100));

    signal(&db, "reader-ready");
    await_signal(&db, "go");

    for n in 0..2_000 {
        assert_eq!(reader.get(&key(n)).unwrap().unwrap(), value(n, 100));
    }
    assert_eq!(reader.len().unwrap(), 2_000);
}

#[test]
fn many_reader_processes_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path();

    let writer = DbRW::<32>::open_or_create(db, &Options::new().buckets([32, 64])).unwrap();
    for n in 0..200 {
        writer
            .insert(&key(n), &value(n, if n % 2 == 0 { 32 } else { 64 }))
            .unwrap();
    }

    let readers: Vec<_> = (0..4)
        .map(|_| spawn_reader("child_reads_all", db))
        .collect();

    // Keep writing while all four read.
    for n in 200..400 {
        writer
            .insert(&key(n), &value(n, if n % 2 == 0 { 32 } else { 64 }))
            .unwrap();
    }

    for mut reader in readers {
        assert!(reader.wait().unwrap().success(), "reader process failed");
    }
    assert_eq!(writer.len().unwrap(), 400);
}

#[test]
fn child_reads_all() {
    let Some(db) = assigned_db() else { return };

    let reader = DbRO::<32>::open(&db).unwrap();
    for _ in 0..20 {
        for n in 0..200 {
            assert_eq!(
                reader.get(&key(n)).unwrap().unwrap(),
                value(n, if n % 2 == 0 { 32 } else { 64 })
            );
        }
    }
}

#[test]
fn reader_process_open_or_creates_against_a_locked_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path();

    // The writer holds the exclusive lock for the whole test. The reader must
    // still get through: the database exists, so nothing has to be created.
    let writer = DbRW::<32>::open_or_create(db, &Options::new().bucket(48)).unwrap();
    writer.insert(&key(1), &value(1, 48)).unwrap();

    let mut reader = spawn_reader("child_open_or_creates_against_a_locked_database", db);
    assert!(reader.wait().unwrap().success(), "reader process failed");
}

#[test]
fn child_open_or_creates_against_a_locked_database() {
    let Some(db) = assigned_db() else { return };

    // One MDBX environment per path per process, so the mismatching attempt
    // goes first and its handle is gone before the real one is opened.
    assert!(matches!(
        DbRO::<32>::open_or_create(&db, &Options::new()),
        Err(bytesklad::Error::BucketsMismatch { .. })
    ));

    let reader = DbRO::<32>::open_or_create(&db, &Options::new().bucket(48)).unwrap();
    assert_eq!(reader.get(&key(1)).unwrap().unwrap(), value(1, 48));
}
