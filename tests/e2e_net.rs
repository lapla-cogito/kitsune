//! End-to-end virtio-net (TAP + guest ping + offload negotiation).

#[path = "support/guest.rs"]
mod guest;
#[path = "support/kvm.rs"]
mod kvm;
#[path = "support/run_tap.rs"]
mod run_tap;

/// Ping the host TAP and check CSUM/TSO feature negotiation in one guest boot.
#[test]
fn boot_with_tap_ping_and_offloads() {
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
        &[
            "kitsune-initrd-ok",
            "kitsune-net-offload-ok",
            "kitsune-net-ok",
        ],
    );
    run_tap::assert_contains(&out, "kitsune-initrd-ok");
    run_tap::assert_contains(&out, "kitsune-net-offload-ok");
    run_tap::assert_contains(&out, "kitsune-net-ok");
    assert!(
        !out.contains("kitsune-net-offload-fail"),
        "guest reported offload negotiation failure:\n{out}"
    );
    assert!(
        out.lines().any(|l| {
            l.strip_prefix("kitsune-net-features=")
                .is_some_and(|f| f.len() >= 14 && f.chars().all(|c| c == '0' || c == '1'))
        }),
        "missing or invalid kitsune-net-features line:\n{out}"
    );
}
