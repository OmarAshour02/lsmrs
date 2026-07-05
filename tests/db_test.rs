use lsmrs::{Db, config::Config};

#[test]
fn put_get_delete_roundtrip() {
    let path = std::env::temp_dir()
        .join(format!("lsmrs_it_{}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let config = Config {
        path,
        sync: false,
        ..Default::default()
    };
    let mut db = Db::with_config(config).unwrap();
    let _x = db.put(b"alpha".to_vec(), b"1".to_vec());
    let _y = db.put(b"beta".to_vec(), b"2".to_vec());

    assert_eq!(db.get(b"alpha"), Some(b"1".to_vec()));
    assert_eq!(db.get(b"beta"), Some(b"2".to_vec()));

    assert!(db.delete(b"alpha").unwrap().is_some());
    assert_eq!(db.get(b"alpha"), None);
    assert_eq!(db.get(b"beta"), Some(b"2".to_vec()));
}
