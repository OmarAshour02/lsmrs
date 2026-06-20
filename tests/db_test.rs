use lsmrs::{Db, config::Config};

#[test]
fn put_get_delete_roundtrip() {
    let config = Config::default();
    let mut db = Db::open(config.path.as_str(), config.sync).unwrap();
    let _x = db.put(b"alpha".to_vec(), b"1".to_vec());
    let _y = db.put(b"beta".to_vec(), b"2".to_vec());

    assert_eq!(db.get(b"alpha"), Some(b"1".to_vec()));
    assert_eq!(db.get(b"beta"), Some(b"2".to_vec()));

    assert!(db.delete(b"alpha").unwrap().is_some());
    assert_eq!(db.get(b"alpha"), None);
    assert_eq!(db.get(b"beta"), Some(b"2".to_vec()));
}
