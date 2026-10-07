//! The files in contrib/ that the package and contrib/install.sh install.

use std::path::Path;

/// The attach unit points to this project's repository (1.1.0 still had
/// its old address) and to the man page.
#[test]
fn the_attach_unit_documents_this_project() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd/storage-spaces-attach.service");
    let unit = std::fs::read_to_string(path).unwrap();
    let docs: Vec<&str> = unit
        .lines()
        .filter_map(|l| l.strip_prefix("Documentation="))
        .flat_map(|l| l.split(' '))
        .collect();
    assert!(docs.contains(&env!("CARGO_PKG_REPOSITORY")), "{docs:?}");
    assert!(docs.contains(&"man:spaces(8)"), "{docs:?}");
}
