//! End-to-end virtio-net (TAP + guest ping).

#[path = "support/guest.rs"]
mod guest;
#[path = "support/kvm.rs"]
mod kvm;
#[path = "support/run_tap.rs"]
mod run_tap;

#[test]
fn boot_with_tap_ping_host() {
    kvm::require_kvm();
    guest::prepare_guest();
    let dir = guest::guest_dir();
    let kernel = dir.join("vmlinux");
    let initrd = dir.join("initrd.img");

    let out = run_tap::run_until_with_tap(
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
    run_tap::assert_contains(&out, "kitsune-initrd-ok");
    run_tap::assert_contains(&out, "kitsune-net-ok");
}
