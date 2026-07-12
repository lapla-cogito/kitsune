//! End-to-end virtio-net (TAP + guest ping).

#[path = "common/mod.rs"]
mod common;

fn ensure_guest() {
    let dir = common::guest_dir();
    // Always refresh initrd when prepare script stamp is missing (net markers).
    let status = std::process::Command::new("bash")
        .args(["scripts/ci_prepare_guest.sh", dir.to_str().unwrap()])
        .status()
        .expect("run ci_prepare_guest.sh");
    assert!(status.success(), "ci_prepare_guest.sh failed");
}

#[test]
fn boot_with_tap_ping_host() {
    common::require_kvm();
    ensure_guest();
    let dir = common::guest_dir();
    let kernel = dir.join("vmlinux");
    let initrd = dir.join("initrd.img");

    let out = common::run_until_with_tap(
        &[
            "run",
            "--kernel",
            kernel.to_str().unwrap(),
            "--initrd",
            initrd.to_str().unwrap(),
            "--memory",
            "512",
            "--cmdline",
            "console=ttyS0 reboot=k panic=1 pci=off nomodule",
        ],
        std::time::Duration::from_secs(90),
        &["kitsune-initrd-ok", "kitsune-net-ok"],
    );
    common::assert_contains(&out, "kitsune-initrd-ok");
    common::assert_contains(&out, "kitsune-net-ok");
}
