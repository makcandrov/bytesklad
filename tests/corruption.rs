use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use bytesklad::{DbRO, DbRW, DbRead, DbWrite, Error, Options};

fn bucket_paths(root: &Path) -> [PathBuf; 3] {
    let store = root.join("store");
    [
        store.join("unsized.bucket"),
        store.join("sized-0000000008.bucket"),
        store.join("sized-0000000016.bucket"),
    ]
}

fn create_populated_database() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new().buckets([8, 16])).unwrap();
    db.insert(&[1; 32], b"unsized").unwrap();
    db.insert(&[2; 32], b"bucket-1").unwrap();
    db.insert(&[3; 32], &[3; 16]).unwrap();
    drop(db);
    dir
}

fn write_checkpoint(root: &Path, entries: &[(u32, u64)]) {
    let mut bytes = b"SKLDCKPT".to_vec();
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for &(tag, frontier) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&frontier.to_le_bytes());
    }
    fs::write(root.join("store/checkpoint"), bytes).unwrap();
}

#[test]
fn missing_checkpoint_preserves_committed_bucket_bytes() {
    let dir = create_populated_database();
    let paths = bucket_paths(dir.path());
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    fs::remove_file(dir.path().join("store/checkpoint")).unwrap();

    assert!(matches!(
        DbRW::<32>::open(dir.path()),
        Err(Error::Corrupt { .. })
    ));
    for (path, expected) in paths.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), expected);
    }
    let reader = DbRO::<32>::open(dir.path()).unwrap();
    assert_eq!(reader.get(&[1; 32]).unwrap().unwrap(), b"unsized");
    assert_eq!(reader.get(&[2; 32]).unwrap().unwrap(), b"bucket-1");
    assert_eq!(reader.get(&[3; 32]).unwrap().unwrap(), [3; 16]);
}

#[test]
fn incomplete_checkpoint_is_rejected_before_any_bucket_is_truncated() {
    let dir = create_populated_database();
    let paths = bucket_paths(dir.path());
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    // A sequential recovery would erase the first bucket before discovering
    // that the remaining two bucket tags have no durable frontier.
    write_checkpoint(dir.path(), &[(255, 0)]);

    assert!(matches!(
        DbRW::<32>::open(dir.path()),
        Err(Error::Corrupt { .. })
    ));
    for (path, expected) in paths.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), expected);
    }
}

#[test]
fn duplicate_registry_bucket_sizes_are_rejected_by_readers_and_writers() {
    let dir = create_populated_database();
    let paths = bucket_paths(dir.path());
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    let path = dir.path().join("store/registry");
    let mut registry = fs::read(&path).unwrap();
    // The two entries after the 24-byte header now name the same file.
    registry[32..40].copy_from_slice(&8u64.to_le_bytes());
    fs::write(path, registry).unwrap();

    assert!(matches!(
        DbRW::<32>::open(dir.path()),
        Err(Error::Corrupt { .. })
    ));
    assert!(matches!(
        DbRO::<32>::open(dir.path()),
        Err(Error::Corrupt { .. })
    ));
    for (path, expected) in paths.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), expected);
    }
}

#[test]
fn missing_registry_is_not_recreated_over_existing_database() {
    let dir = create_populated_database();
    let paths = bucket_paths(dir.path());
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    let checkpoint_path = dir.path().join("store/checkpoint");
    let checkpoint = fs::read(&checkpoint_path).unwrap();
    let registry_path = dir.path().join("store/registry");
    fs::remove_file(&registry_path).unwrap();

    assert!(matches!(
        DbRW::<32>::open_or_create(dir.path(), &Options::new().buckets([8, 16])),
        Err(Error::Corrupt { .. })
    ));
    assert!(!registry_path.exists());
    assert_eq!(fs::read(checkpoint_path).unwrap(), checkpoint);
    for (path, expected) in paths.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), expected);
    }
}

#[test]
fn oversized_metadata_is_rejected_without_modifying_buckets() {
    let dir = create_populated_database();
    let paths = bucket_paths(dir.path());
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    for name in ["registry", "checkpoint"] {
        let path = dir.path().join("store").join(name);
        let original = fs::read(&path).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[0; 4096])
            .unwrap();
        assert!(matches!(
            DbRW::<32>::open(dir.path()),
            Err(Error::Corrupt { .. })
        ));
        for (bucket_path, expected) in paths.iter().zip(&before) {
            assert_eq!(&fs::read(bucket_path).unwrap(), expected);
        }
        fs::write(path, original).unwrap();
    }
}

#[test]
fn invalid_checkpoint_tags_and_frontiers_do_not_modify_data() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();
    db.insert(&[1; 32], b"unsized").unwrap();
    drop(db);
    let path = dir.path().join("store/unsized.bucket");
    let before = fs::read(&path).unwrap();
    assert_eq!(before.len(), 8);

    for entries in [
        vec![(511, 8)], // Would alias tag 255 after an unchecked u8 cast.
        vec![(0, 8)],
        vec![(255, 8), (255, 8)],
        vec![(255, 9)], // Claims durable data beyond the actual file.
        vec![(255, u64::MAX)],
    ] {
        write_checkpoint(dir.path(), &entries);
        assert!(
            matches!(DbRW::<32>::open(dir.path()), Err(Error::Corrupt { .. })),
            "accepted invalid checkpoint {entries:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn initial_checkpoint_allows_recovery_of_an_interrupted_first_batch() {
    let dir = tempfile::tempdir().unwrap();
    drop(DbRW::<32>::open_or_create(dir.path(), &Options::new().bucket(8)).unwrap());
    assert!(dir.path().join("store/checkpoint").is_file());
    let paths = bucket_paths(dir.path());
    // Neither partial record was committed to the index.
    for path in &paths[..2] {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(&[16, 1, 2])
            .unwrap();
    }

    let db = DbRW::<32>::open(dir.path()).unwrap();
    assert_eq!(db.len().unwrap(), 0);
    for path in &paths[..2] {
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
    }
    assert!(db.insert(&[1; 32], b"unsized").unwrap());
    assert_eq!(db.get(&[1; 32]).unwrap().unwrap(), b"unsized");
}

#[test]
fn malformed_record_prefixes_return_errors_through_the_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();
    db.insert(&[1; 32], b"unsized").unwrap();
    drop(db);
    let path = dir.path().join("store/unsized.bucket");
    for bytes in [
        vec![0x80],     // Truncated varint.
        vec![0xff; 10], // No terminating byte.
        vec![0x40],     // A 64-byte payload cannot fit in a one-byte file.
        vec![0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02],
    ] {
        fs::write(&path, &bytes).unwrap();
        let reader = DbRO::<32>::open(dir.path()).unwrap();
        assert!(
            matches!(reader.get(&[1; 32]), Err(Error::Corrupt { .. })),
            "accepted malformed record {bytes:?}"
        );
    }
}

#[test]
fn stale_temporary_files_are_removed_by_the_next_writer() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    fs::create_dir_all(&store).unwrap();
    // A crash during creation, before the registry was renamed into place.
    fs::write(store.join("registry.tmp.4242.0"), b"SKLDREGY").unwrap();

    drop(DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap());
    assert!(!store.join("registry.tmp.4242.0").exists());

    fs::write(store.join("checkpoint.tmp.4242.0"), b"SKLDCKPT").unwrap();
    drop(DbRW::<32>::open(dir.path()).unwrap());
    assert!(!store.join("checkpoint.tmp.4242.0").exists());
}
