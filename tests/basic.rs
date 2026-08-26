use std::sync::{Arc, Barrier};

use bytesklad::{DbRO, DbRW, DbRead, DbWrite, Error, Options};

fn key(n: u64) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[24..].copy_from_slice(&n.to_be_bytes());
    key
}

fn value(n: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| (n as u8).wrapping_add(i as u8)).collect()
}

fn count_segments(root: &std::path::Path, tag: u8) -> usize {
    let dir = root.join("store").join(format!("b{tag:03}"));
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "seg"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn insert_and_get_without_buckets() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();

    assert!(db.buckets().is_empty());

    for n in 0..64 {
        assert!(db.insert(&key(n), &value(n, n as usize % 200)).unwrap());
    }

    for n in 0..64 {
        assert_eq!(
            db.get(&key(n)).unwrap().unwrap(),
            value(n, n as usize % 200)
        );
    }
    assert_eq!(db.len().unwrap(), 64);
    assert!(db.get(&key(999)).unwrap().is_none());
}

#[test]
fn insert_and_get_with_buckets() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new().buckets([32, 64])).unwrap();

    assert_eq!(db.buckets(), &[32, 64]);

    let sizes = [32usize, 64, 0, 1, 33, 4096];
    for (n, &len) in sizes.iter().enumerate() {
        let n = n as u64;
        assert!(db.insert(&key(n), &value(n, len)).unwrap());
    }
    for (n, &len) in sizes.iter().enumerate() {
        let n = n as u64;
        assert_eq!(db.get(&key(n)).unwrap().unwrap(), value(n, len));
    }

    // The two bucketed lengths went to their own buckets, everything else to
    // the variable-length one.
    assert_eq!(count_segments(dir.path(), 1), 1);
    assert_eq!(count_segments(dir.path(), 2), 1);
}

#[test]
fn duplicate_insert_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();

    assert!(db.insert(&key(1), b"first").unwrap());
    assert!(!db.insert(&key(1), b"second").unwrap());
    assert_eq!(db.get(&key(1)).unwrap().unwrap(), b"first");
    assert_eq!(db.len().unwrap(), 1);
}

#[test]
fn batch_skips_duplicates_within_itself() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();

    let entries = [
        (key(1), b"a".to_vec()),
        (key(2), b"bb".to_vec()),
        (key(1), b"ccc".to_vec()),
    ];
    let inserted = db
        .insert_batch(entries.iter().map(|(k, v)| (k, v.as_slice())))
        .unwrap();

    assert_eq!(inserted, 2);
    assert_eq!(db.get(&key(1)).unwrap().unwrap(), b"a");
}

#[test]
fn empty_batch_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();
    assert_eq!(db.insert_batch(std::iter::empty()).unwrap(), 0);
    assert!(db.is_empty().unwrap());
}

#[test]
fn ordered_navigation() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();

    for n in [10u64, 20, 30] {
        db.insert(&key(n), &value(n, 40)).unwrap();
    }

    assert_eq!(db.first().unwrap().unwrap().0, key(10));
    assert_eq!(db.last().unwrap().unwrap().0, key(30));

    assert_eq!(db.nearest_lower(&key(20)).unwrap().unwrap().0, key(20));
    assert_eq!(db.nearest_lower(&key(25)).unwrap().unwrap().0, key(20));
    assert_eq!(db.nearest_upper(&key(25)).unwrap().unwrap().0, key(30));
    assert_eq!(db.nearest_upper(&key(30)).unwrap().unwrap().0, key(30));

    assert!(db.nearest_lower(&key(5)).unwrap().is_none());
    assert!(db.nearest_upper(&key(35)).unwrap().is_none());
}

#[test]
fn nearest_lower_above_all_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();

    db.insert(&key(1), b"one").unwrap();
    db.insert(&key(2), b"two").unwrap();

    let (found, data) = db.nearest_lower(&[0xff; 32]).unwrap().unwrap();
    assert_eq!(found, key(2));
    assert_eq!(data, b"two");
}

#[test]
fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();

    {
        let db = DbRW::<32>::open_or_create(dir.path(), &Options::new().bucket(48)).unwrap();
        for n in 0..32 {
            db.insert(&key(n), &value(n, if n % 2 == 0 { 48 } else { 77 }))
                .unwrap();
        }
    }

    // Reopened with no options at all: the bucket layout comes back from disk.
    let db = DbRW::<32>::open(dir.path()).unwrap();
    assert_eq!(db.buckets(), &[48]);
    for n in 0..32 {
        assert_eq!(
            db.get(&key(n)).unwrap().unwrap(),
            value(n, if n % 2 == 0 { 48 } else { 77 })
        );
    }
}

#[test]
fn segments_roll_over_and_stay_readable() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new().segment_size(1024).bucket(100))
        .unwrap();

    for n in 0..100 {
        db.insert(&key(n), &value(n, 100)).unwrap();
    }
    for n in 0..100 {
        assert_eq!(db.get(&key(n)).unwrap().unwrap(), value(n, 100));
    }

    // 10 records of 100 bytes per 1 KiB segment, and no record straddles a
    // segment boundary.
    assert_eq!(count_segments(dir.path(), 1), 10);
}

#[test]
fn record_larger_than_a_segment_gets_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new().segment_size(1024)).unwrap();

    db.insert(&key(1), &value(1, 10)).unwrap();
    db.insert(&key(2), &value(2, 5000)).unwrap();
    db.insert(&key(3), &value(3, 10)).unwrap();

    assert_eq!(db.get(&key(1)).unwrap().unwrap(), value(1, 10));
    assert_eq!(db.get(&key(2)).unwrap().unwrap(), value(2, 5000));
    assert_eq!(db.get(&key(3)).unwrap().unwrap(), value(3, 10));
    assert_eq!(count_segments(dir.path(), 0), 3);
}

#[test]
fn values_spanning_the_probe_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();

    // Straddles the 512-byte speculative read in both directions, and crosses
    // the one-to-two byte length-prefix boundary.
    for (n, len) in [126usize, 127, 128, 129, 509, 510, 511, 512, 513, 1024]
        .into_iter()
        .enumerate()
    {
        db.insert(&key(n as u64), &value(n as u64, len)).unwrap();
    }
    for (n, len) in [126usize, 127, 128, 129, 509, 510, 511, 512, 513, 1024]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            db.get(&key(n as u64)).unwrap().unwrap(),
            value(n as u64, len)
        );
    }
}

#[test]
fn concurrent_writers_and_readers_in_one_process() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        DbRW::<32>::open_or_create(dir.path(), &Options::new().buckets([32, 64])).unwrap(),
    );

    // Pre-populate so the readers have something to hammer on.
    for n in 0..50 {
        db.insert(&key(n), &value(n, 64)).unwrap();
    }

    let barrier = Arc::new(Barrier::new(6));
    let mut handles = Vec::new();

    for w in 0..2u64 {
        let db = db.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for i in 0..200u64 {
                let n = 1_000 + w * 1_000 + i;
                db.insert(&key(n), &value(n, 32 + (i as usize % 90)))
                    .unwrap();
            }
        }));
    }

    for _ in 0..3 {
        let db = db.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..200 {
                for n in 0..50 {
                    assert_eq!(db.get(&key(n)).unwrap().unwrap(), value(n, 64));
                }
            }
        }));
    }

    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(db.len().unwrap(), 450);
    for w in 0..2u64 {
        for i in 0..200u64 {
            let n = 1_000 + w * 1_000 + i;
            assert_eq!(
                db.get(&key(n)).unwrap().unwrap(),
                value(n, 32 + (i as usize % 90))
            );
        }
    }
}

#[test]
fn only_one_writer_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let _db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();
    assert!(matches!(DbRW::<32>::open(dir.path()), Err(Error::Locked)));
}

#[test]
fn key_length_is_fixed_at_creation() {
    let dir = tempfile::tempdir().unwrap();
    drop(DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap());

    assert!(matches!(
        DbRW::<20>::open(dir.path()),
        Err(Error::KeyLenMismatch {
            stored: 32,
            requested: 20
        })
    ));
}

#[test]
fn keys_of_other_lengths() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbRW::<8>::open_or_create(dir.path(), &Options::new()).unwrap();

    db.insert(&[1, 2, 3, 4, 5, 6, 7, 8], b"eight-byte key")
        .unwrap();
    assert_eq!(
        db.get(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap().unwrap(),
        b"eight-byte key"
    );
}

#[test]
fn bytes_written_past_the_checkpoint_are_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let segment = dir.path().join("store").join("b000").join("0000000000.seg");

    {
        let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();
        for n in 0..16 {
            db.insert(&key(n), &value(n, 30)).unwrap();
        }
    }
    let committed = std::fs::metadata(&segment).unwrap().len();

    // A crash in the middle of an append leaves bytes past the frontier the
    // checkpoint published.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&segment)
            .unwrap();
        file.write_all(&[0xab; 500]).unwrap();
    }
    assert_eq!(std::fs::metadata(&segment).unwrap().len(), committed + 500);

    let db = DbRW::<32>::open(dir.path()).unwrap();
    assert_eq!(std::fs::metadata(&segment).unwrap().len(), committed);

    for n in 0..16 {
        assert_eq!(db.get(&key(n)).unwrap().unwrap(), value(n, 30));
    }
    // The recovered store keeps accepting writes at the right offset.
    db.insert(&key(99), &value(99, 30)).unwrap();
    assert_eq!(db.get(&key(99)).unwrap().unwrap(), value(99, 30));
}

#[test]
fn segments_past_the_checkpoint_are_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("store").join("b000");

    {
        let db = DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap();
        db.insert(&key(1), b"kept").unwrap();
    }

    // A segment the writer had rolled into but never checkpointed.
    std::fs::write(bucket.join("0000000007.seg"), [0xcd; 64]).unwrap();
    assert_eq!(count_segments(dir.path(), 0), 2);

    let db = DbRW::<32>::open(dir.path()).unwrap();
    assert_eq!(count_segments(dir.path(), 0), 1);
    assert_eq!(db.get(&key(1)).unwrap().unwrap(), b"kept");
}

#[test]
fn recovery_is_per_bucket() {
    let dir = tempfile::tempdir().unwrap();

    {
        let db = DbRW::<32>::open_or_create(dir.path(), &Options::new().buckets([32, 64])).unwrap();
        db.insert(&key(1), &value(1, 32)).unwrap();
        db.insert(&key(2), &value(2, 64)).unwrap();
        db.insert(&key(3), &value(3, 7)).unwrap();
    }

    // Junk in every bucket at once.
    for tag in 0..3u8 {
        use std::io::Write;
        let path = dir
            .path()
            .join("store")
            .join(format!("b{tag:03}"))
            .join("0000000000.seg");
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(&[0xff; 128]).unwrap();
    }

    let db = DbRW::<32>::open(dir.path()).unwrap();
    assert_eq!(db.get(&key(1)).unwrap().unwrap(), value(1, 32));
    assert_eq!(db.get(&key(2)).unwrap().unwrap(), value(2, 64));
    assert_eq!(db.get(&key(3)).unwrap().unwrap(), value(3, 7));

    for tag in 0..3u8 {
        let path = dir
            .path()
            .join("store")
            .join(format!("b{tag:03}"))
            .join("0000000000.seg");
        let len = std::fs::metadata(path).unwrap().len();
        assert_eq!(len, [7 + 1, 32, 64][tag as usize] as u64);
    }
}

#[test]
fn open_requires_an_existing_database() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        DbRW::<32>::open(dir.path().join("nope")),
        Err(Error::NotInitialized)
    ));
    assert!(matches!(
        DbRO::<32>::open(dir.path().join("nope")),
        Err(Error::NotInitialized)
    ));

    // An empty directory is not a database either.
    assert!(matches!(
        DbRW::<32>::open(dir.path()),
        Err(Error::NotInitialized)
    ));
}

#[test]
fn open_reads_the_configuration_back_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    drop(
        DbRW::<32>::open_or_create(
            dir.path(),
            &Options::new().segment_size(4096).buckets([32, 64]),
        )
        .unwrap(),
    );

    let db = DbRW::<32>::open(dir.path()).unwrap();
    assert_eq!(db.buckets(), &[32, 64]);
    assert_eq!(db.segment_size(), 4096);
    drop(db);

    let db = DbRO::<32>::open(dir.path()).unwrap();
    assert_eq!(db.buckets(), &[32, 64]);
    assert_eq!(db.segment_size(), 4096);
}

#[test]
fn open_or_create_creates_then_matches() {
    let dir = tempfile::tempdir().unwrap();

    {
        let db = DbRW::<32>::open_or_create(dir.path(), &Options::new().buckets([32, 64])).unwrap();
        assert_eq!(db.buckets(), &[32, 64]);
        db.insert(&key(1), &value(1, 64)).unwrap();
    }

    // Order is not part of the assertion: it only fixes internal tags.
    let db = DbRW::<32>::open_or_create(dir.path(), &Options::new().buckets([64, 32])).unwrap();
    assert_eq!(db.buckets(), &[32, 64]);
    assert_eq!(db.get(&key(1)).unwrap().unwrap(), value(1, 64));
}

#[test]
fn open_or_create_rejects_bucket_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    drop(DbRW::<32>::open_or_create(dir.path(), &Options::new().buckets([32, 64])).unwrap());

    for requested in [vec![32usize], vec![32, 64, 96], vec![]] {
        let err = DbRW::<32>::open_or_create(
            dir.path(),
            &Options::new().buckets(requested.iter().copied()),
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::BucketsMismatch { stored, requested: r }
                if stored == &[32, 64] && r == &requested),
            "unexpected error for {requested:?}: {err}"
        );
    }
}

#[test]
fn open_or_create_rejects_key_length_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    drop(DbRW::<32>::open_or_create(dir.path(), &Options::new()).unwrap());

    assert!(matches!(
        DbRW::<20>::open_or_create(dir.path(), &Options::new()),
        Err(Error::KeyLenMismatch {
            stored: 32,
            requested: 20
        })
    ));
}

#[test]
fn open_or_create_rejects_segment_size_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    drop(DbRW::<32>::open_or_create(dir.path(), &Options::new().segment_size(4096)).unwrap());

    // Leaving it unset asserts the default, so it mismatches just the same.
    for options in [Options::new().segment_size(8192), Options::new()] {
        assert!(matches!(
            DbRW::<32>::open_or_create(dir.path(), &options),
            Err(Error::SegmentSizeMismatch { stored: 4096, .. })
        ));
    }
}

#[test]
fn read_only_open_or_create_bootstraps_a_missing_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh");

    // Only one MDBX environment per path per process, so each handle is
    // dropped before the next is opened.
    {
        let reader = DbRO::<32>::open_or_create(&path, &Options::new().bucket(48)).unwrap();
        assert_eq!(reader.buckets(), &[48]);
        assert!(reader.is_empty().unwrap());
    }

    // The creation released the writer lock, so a real writer can take it.
    {
        let writer = DbRW::<32>::open(&path).unwrap();
        writer.insert(&key(7), &value(7, 48)).unwrap();
    }

    let reader = DbRO::<32>::open(&path).unwrap();
    assert_eq!(reader.get(&key(7)).unwrap().unwrap(), value(7, 48));
}

#[test]
fn read_only_open_or_create_matches_an_existing_database() {
    let dir = tempfile::tempdir().unwrap();
    {
        let writer = DbRW::<32>::open_or_create(dir.path(), &Options::new().bucket(48)).unwrap();
        writer.insert(&key(7), &value(7, 48)).unwrap();
    }

    {
        let reader = DbRO::<32>::open_or_create(dir.path(), &Options::new().bucket(48)).unwrap();
        assert_eq!(reader.get(&key(7)).unwrap().unwrap(), value(7, 48));
    }

    assert!(matches!(
        DbRO::<32>::open_or_create(dir.path(), &Options::new()),
        Err(Error::BucketsMismatch { .. })
    ));
    assert!(matches!(
        DbRO::<20>::open_or_create(dir.path(), &Options::new().bucket(48)),
        Err(Error::KeyLenMismatch {
            stored: 32,
            requested: 20
        })
    ));
}

#[test]
fn zero_sized_options_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        DbRW::<32>::open_or_create(dir.path(), &Options::new().bucket(0)),
        Err(Error::ZeroBucket)
    ));
    assert!(matches!(
        DbRW::<32>::open_or_create(dir.path(), &Options::new().segment_size(0)),
        Err(Error::ZeroSegmentSize)
    ));
}
