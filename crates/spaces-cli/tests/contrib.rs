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

/// The udev rule imports what attach writes for the devices of attached
/// spaces, and hides from udisks2 the devices a space is served through
/// (with their kernel partitions, which duplicate the space's partition
/// devices) and the devices of spaces attached past their verdict.
#[test]
fn the_udev_rule_hides_what_nothing_should_mount_unasked() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/udev/69-storage-spaces.rules");
    let rule = std::fs::read_to_string(path).unwrap().replace("\\\n", "");
    let lines: Vec<&str> = rule
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect();
    let has = |parts: &[&str]| lines.iter().any(|l| parts.iter().all(|p| l.contains(p)));
    assert!(
        has(&[
            "KERNEL==\"dm-*\"",
            "ENV{DM_NAME}==\"ss-*\"",
            "IMPORT{file}=\"/run/storage-spaces/udev/$env{DM_NAME}.props\""
        ]),
        "{rule}"
    );
    assert!(
        has(&[
            "ENV{DEVTYPE}==\"disk\"",
            "IMPORT{file}=\"/run/storage-spaces/udev/$kernel.props\""
        ]),
        "{rule}"
    );
    assert!(
        has(&[
            "ENV{DEVTYPE}==\"partition\"",
            "IMPORT{file}=\"/run/storage-spaces/udev/$parent.props\""
        ]),
        "{rule}"
    );
    assert!(has(&["ENV{SS_BACKEND}==\"1\"", "ENV{UDISKS_IGNORE}=\"1\""]), "{rule}");
    assert!(
        has(&["ENV{SS_VERDICT}!=\"healthy\"", "ENV{UDISKS_IGNORE}=\"1\""]),
        "{rule}"
    );
    // The attach on member disks stays.
    assert!(
        has(&["e75caf8f-f680-4cee-afa3-b001e56efc2d", "SYSTEMD_WANTS"]),
        "{rule}"
    );
}
