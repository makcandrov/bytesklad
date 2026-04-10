use std::{fs, path::PathBuf};

use flat_store::FlatStore;

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flat_store_test_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn single_file_roundtrip() {
    let dir = tmp_dir("single");
    let store = FlatStore::open(&dir, vec![]).unwrap();

    let data = b"hello world";
    let r = store.insert(data).unwrap();
    assert_eq!(store.read_to_vec(r, data.len()).unwrap(), data);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn split_files_roundtrip() {
    let dir = tmp_dir("split");
    let store = FlatStore::open(&dir, [4, 8]).unwrap();

    let d4 = &[1u8, 2, 3, 4];
    let d8 = &[10u8, 20, 30, 40, 50, 60, 70, 80];
    let dv = b"variable length data";

    let r4 = store.insert(d4).unwrap();
    let r8 = store.insert(d8).unwrap();
    let rv = store.insert(dv.as_slice()).unwrap();

    assert_eq!(store.read_to_vec(r4, 4).unwrap(), d4);
    assert_eq!(store.read_to_vec(r8, 8).unwrap(), d8);
    assert_eq!(store.read_to_vec(rv, dv.len()).unwrap(), dv);

    let _ = fs::remove_dir_all(&dir);
}
