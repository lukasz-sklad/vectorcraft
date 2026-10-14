#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use vectorcraft_engine::cmd::fileio::{detect, load};

#[test]
fn test_cdr_detection() {
    assert!(detect("logo.cdr", b"RIFF\x00\x00\x00\x00CDR9").is_some());
    let fmt = detect("logo.cdr", b"RIFF\x00\x00\x00\x00CDR9").unwrap();
    assert_eq!(fmt.id, "cdr");
    assert_eq!(fmt.label, "CorelDRAW");
}

#[test]
fn test_cdr_load_real_file() {
    let path = std::path::Path::new("/home/deck/Pictures/cdr/2010_KAL.cdr");
    if let Ok(bytes) = std::fs::read(path) {
        let res = load("2010_KAL.cdr", &bytes);
        assert!(res.is_ok(), "failed to load 2010_KAL.cdr: {:?}", res.err());
        let loaded = res.unwrap();
        assert!(!loaded.doc.layers.is_empty());
        println!("Successfully loaded 2010_KAL.cdr! Warnings: {:?}", loaded.warnings);
        let desktop_layer = loaded.doc.layers.iter().find(|l| l.name.as_deref() == Some("Desktop"));
        assert!(desktop_layer.is_some(), "Desktop layer should be present");
        let children = desktop_layer.unwrap().children().unwrap();
        assert!(!children.is_empty(), "Desktop layer should contain imported objects");
        assert!(children.iter().any(|c| matches!(&c.kind, vectorcraft_doc::NodeKind::Text(_))));
        assert!(children.iter().any(|c| matches!(&c.kind, vectorcraft_doc::NodeKind::Path { .. })));
    }
}
