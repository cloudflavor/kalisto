const ROOTFS_SCRIPT: &str = include_str!("../scripts/image/build_lima_rootfs_in_container.sh");
const FINALIZE_SCRIPT: &str = include_str!("../scripts/macos/finalize_lima_node_image.sh");
const ASSEMBLY_SCRIPT: &str = include_str!("../scripts/macos/build_lima_node_image.sh");

#[test]
fn node_image_installs_lima_boot_dependencies_before_export() {
    assert!(
        ROOTFS_SCRIPT
            .contains("${PACKAGE_LIST} dbus dbus-user-session systemd-resolved rsync lsb-release")
    );
}

#[test]
fn node_image_uses_lima_nocloud_network_configuration() {
    assert!(!ROOTFS_SCRIPT.contains("config: disabled"));
    assert!(!ROOTFS_SCRIPT.contains("DHCP=yes"));
    assert!(!ROOTFS_SCRIPT.contains("dhcp6: true"));
}

#[test]
fn node_image_does_not_wait_for_nonexistent_serial_devices() {
    assert!(
        ROOTFS_SCRIPT
            .lines()
            .filter(|line| line.starts_with("systemctl enable "))
            .all(|line| !line.contains("serial-getty@"))
    );
}

#[test]
fn node_image_has_a_working_dns_resolver_link() {
    assert!(ASSEMBLY_SCRIPT.contains(
        "ln -sfn /run/systemd/resolve/stub-resolv.conf \"${ROOTFS_SOURCE_DIR}/etc/resolv.conf\""
    ));
}

#[test]
fn finalized_image_clears_cloud_init_identity_for_each_new_node() {
    assert!(FINALIZE_SCRIPT.contains("cloud-init clean --logs --seed --machine-id"));
}
