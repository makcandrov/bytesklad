use std::fs;
use std::path::PathBuf;

use preimages_db::{FileGroup, PreimageDb, PreimageDbConfig};
use tiny_keccak::{Hasher, Keccak};

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak::v256();
    hasher.update(data);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("preimages_db_test_{name}_{}", std::process::id()));
    // let _ = fs::remove_dir_all(&dir);
    dir
}

fn open_db(name: &str, groups: Vec<FileGroup>) -> (PreimageDb, PathBuf) {
    let dir = tmp_dir(name);
    let db = PreimageDb::open(PreimageDbConfig::new(dir.clone(), groups)).unwrap();
    (db, dir)
}

#[test]
fn insert_and_get() {
    let (db, dir) = open_db("insert_get", vec![FileGroup::Any]);

    let data = b"hello world";
    let hash = keccak256(data);

    assert!(db.insert(&hash, data).unwrap());
    assert_eq!(db.get(&hash).unwrap().unwrap(), data);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn get_missing_returns_none() {
    let (db, dir) = open_db("get_missing", vec![FileGroup::Any]);

    let hash = keccak256(b"does not exist");
    assert!(db.get(&hash).unwrap().is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn duplicate_insert_returns_false() {
    let (db, dir) = open_db("dup_insert", vec![FileGroup::Any]);

    let data = b"duplicate";
    let hash = keccak256(data);

    assert!(db.insert(&hash, data).unwrap());
    assert!(!db.insert(&hash, data).unwrap());
    assert_eq!(db.len().unwrap(), 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn contains() {
    let (db, dir) = open_db("contains", vec![FileGroup::Any]);

    let data = b"present";
    let hash = keccak256(data);
    let missing = keccak256(b"absent");

    assert!(!db.contains(&hash).unwrap());
    db.insert(&hash, data).unwrap();
    assert!(db.contains(&hash).unwrap());
    assert!(!db.contains(&missing).unwrap());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn len_and_is_empty() {
    let (db, dir) = open_db("len", vec![FileGroup::Any]);

    assert!(db.is_empty().unwrap());
    assert_eq!(db.len().unwrap(), 0);

    let d1 = b"one";
    let d2 = b"two";
    db.insert(&keccak256(d1), d1).unwrap();
    assert_eq!(db.len().unwrap(), 1);
    assert!(!db.is_empty().unwrap());

    db.insert(&keccak256(d2), d2).unwrap();
    assert_eq!(db.len().unwrap(), 2);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn batch_insert() {
    let (db, dir) = open_db("batch", vec![FileGroup::Any]);

    let entries: Vec<([u8; 32], Vec<u8>)> = (0..100u32)
        .map(|i| {
            let data = i.to_le_bytes().to_vec();
            (keccak256(&data), data)
        })
        .collect();

    let inserted = db.insert_batch(&entries).unwrap();
    assert_eq!(inserted, 100);
    assert_eq!(db.len().unwrap(), 100);

    for (hash, data) in &entries {
        assert_eq!(db.get(hash).unwrap().unwrap(), *data);
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn batch_insert_deduplicates() {
    let (db, dir) = open_db("batch_dedup", vec![FileGroup::Any]);

    let data = b"already here";
    let hash = keccak256(data);
    db.insert(&hash, data).unwrap();

    let entries = vec![(hash, data.to_vec()), (keccak256(b"new"), b"new".to_vec())];

    let inserted = db.insert_batch(&entries).unwrap();
    assert_eq!(inserted, 1);
    assert_eq!(db.len().unwrap(), 2);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn batch_insert_empty() {
    let (db, dir) = open_db("batch_empty", vec![FileGroup::Any]);

    assert_eq!(db.insert_batch(&[]).unwrap(), 0);
    assert!(db.is_empty().unwrap());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn split_files() {
    let groups = vec![FileGroup::Exact(4), FileGroup::Exact(32), FileGroup::Any];
    let (db, dir) = open_db("split", groups);

    let d4 = [0xAAu8; 4];
    let d32 = [0xBBu8; 32];
    let dvar = b"variable length preimage data here";

    let h4 = keccak256(&d4);
    let h32 = keccak256(&d32);
    let hvar = keccak256(dvar);

    db.insert(&h4, &d4).unwrap();
    db.insert(&h32, &d32).unwrap();
    db.insert(&hvar, dvar).unwrap();

    assert_eq!(db.get(&h4).unwrap().unwrap(), d4);
    assert_eq!(db.get(&h32).unwrap().unwrap(), d32);
    assert_eq!(db.get(&hvar).unwrap().unwrap(), dvar);
    assert_eq!(db.len().unwrap(), 3);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reopen_preserves_data() {
    let dir = tmp_dir("reopen");
    let groups = vec![FileGroup::Exact(4), FileGroup::Any];

    let d4 = [0xCCu8; 4];
    let dvar = b"persisted across reopens";
    let h4 = keccak256(&d4);
    let hvar = keccak256(dvar);

    {
        let db = PreimageDb::open(PreimageDbConfig::new(dir.clone(), groups.clone())).unwrap();
        db.insert(&h4, &d4).unwrap();
        db.insert(&hvar, dvar).unwrap();
        db.sync().unwrap();
    }

    let db = PreimageDb::open(PreimageDbConfig::new(dir.clone(), groups)).unwrap();
    assert_eq!(db.len().unwrap(), 2);
    assert_eq!(db.get(&h4).unwrap().unwrap(), d4);
    assert_eq!(db.get(&hvar).unwrap().unwrap(), dvar);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn various_sizes() {
    let (db, dir) = open_db("sizes", vec![FileGroup::Any]);

    let sizes = [0, 1, 31, 32, 33, 100, 1000, 10_000];
    for &size in &sizes {
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let hash = keccak256(&data);
        db.insert(&hash, &data).unwrap();
        assert_eq!(db.get(&hash).unwrap().unwrap(), data);
    }

    assert_eq!(db.len().unwrap(), sizes.len());

    // let _ = fs::remove_dir_all(&dir);
}
