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

/// The attach unit runs before local-fs-pre.target (so that fstab can mount
/// volumes of spaces) and must not order itself after anything that comes
/// after it: PrivateTmp= orders it after systemd-tmpfiles-setup.service,
/// which comes after local-fs.target. 1.1.0 and 1.2.0 had that cycle, and
/// systemd broke it at every boot by dropping jobs, other units' mounts
/// among them.
#[test]
fn the_attach_unit_has_no_ordering_cycle_at_boot() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd/storage-spaces-attach.service");
    let unit = std::fs::read_to_string(path).unwrap();
    let settings: Vec<&str> = unit
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .collect();
    assert!(settings.contains(&"Before=local-fs-pre.target"), "{unit}");
    assert!(settings.contains(&"DefaultDependencies=no"), "{unit}");
    for s in &settings {
        assert!(!s.starts_with("PrivateTmp="), "{s}");
        assert!(!s.starts_with("After=") || !s.contains("local-fs"), "{s}");
    }
    assert!(settings.contains(&"TemporaryFileSystem=/tmp /var/tmp"), "{unit}");
}

/// udev starts the attach unit for every member disk that appears or
/// changes, several times within seconds at boot: under the default start
/// limit (5 in 10 s) a member that came later was not attached.
#[test]
fn the_attach_unit_has_no_start_limit() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd/storage-spaces-attach.service");
    let unit = std::fs::read_to_string(path).unwrap();
    let section = unit.split("[Service]").next().unwrap();
    assert!(section.lines().any(|l| l == "StartLimitIntervalSec=0"), "{unit}");
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
    // The attach on member disks stays, but not for the devices of attached
    // spaces: the line comes after their properties are imported.
    assert!(
        has(&[
            "e75caf8f-f680-4cee-afa3-b001e56efc2d",
            "ENV{SS_SPACE}!=\"?*\"",
            "SYSTEMD_WANTS"
        ]),
        "{rule}"
    );
    let at = |part: &str| lines.iter().position(|l| l.contains(part)).unwrap();
    assert!(at("SYSTEMD_WANTS") > at("$parent.props"), "{rule}");
    assert!(at("SYSTEMD_WANTS") < at("LABEL=\"storage_spaces_end\""), "{rule}");
}
