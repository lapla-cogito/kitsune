//! End-to-end virtio-net (TAP: ping, TCP, bulk, offloads, SMP).

#[path = "support/guest.rs"]
mod guest;
#[path = "support/kvm.rs"]
mod kvm;
#[path = "support/run_tap.rs"]
mod run_tap;

fn net_args(kernel: &std::path::Path, initrd: &std::path::Path, cpus: u8) -> Vec<String> {
    let mut a = vec![
        "run".into(),
        "--kernel".into(),
        kernel.to_str().unwrap().into(),
        "--initrd".into(),
        initrd.to_str().unwrap().into(),
        "--memory".into(),
        "512".into(),
        "--cmdline".into(),
        "console=ttyS0 reboot=k panic=1 pci=off nomodule".into(),
    ];
    if cpus != 1 {
        a.push("--cpus".into());
        a.push(cpus.to_string());
    }
    a
}

fn run_net(cpus: u8, markers: &[&str]) -> String {
    kvm::require_kvm();
    guest::prepare_guest();
    let dir = guest::guest_dir();
    let args = net_args(&dir.join("vmlinux"), &dir.join("initrd.img"), cpus);
    let refs: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
    run_tap::run_until_with_tap(&refs, std::time::Duration::from_secs(120), markers)
}

/// ICMP, TCP to host TAP, bulk pings, and CSUM/TSO feature negotiation.
#[test]
fn boot_with_tap_ping_tcp_bulk_and_offloads() {
    let out = run_net(
        1,
        &[
            "kitsune-initrd-ok",
            "kitsune-net-offload-ok",
            "kitsune-net-ok",
            "kitsune-net-bulk-ok",
            "kitsune-net-tcp-ok",
        ],
    );
    run_tap::assert_contains(&out, "kitsune-initrd-ok");
    run_tap::assert_contains(&out, "kitsune-net-offload-ok");
    run_tap::assert_contains(&out, "kitsune-net-ok");
    run_tap::assert_contains(&out, "kitsune-net-bulk-ok");
    run_tap::assert_contains(&out, "kitsune-net-tcp-ok");
    assert!(
        !out.contains("kitsune-net-offload-fail"),
        "offload negotiation failed:\n{out}"
    );
    assert!(
        !out.contains("kitsune-net-tcp-fail"),
        "guest TCP to host failed:\n{out}"
    );
    assert!(
        !out.contains("kitsune-net-bulk-fail"),
        "bulk ping failed:\n{out}"
    );
    assert!(
        out.lines().any(|l| {
            l.strip_prefix("kitsune-net-features=")
                .is_some_and(|f| f.len() >= 14 && f.chars().all(|c| c == '0' || c == '1'))
        }),
        "missing or invalid kitsune-net-features line:\n{out}"
    );
}

/// Multi-vCPU boot with virtio-net (SMP + datapath worker).
#[test]
fn boot_two_vcpus_with_tap() {
    let out = run_net(
        2,
        &[
            "kitsune-initrd-ok",
            "kitsune-smp-ok",
            "kitsune-net-ok",
            "kitsune-net-tcp-ok",
        ],
    );
    run_tap::assert_contains(&out, "kitsune-initrd-ok");
    run_tap::assert_contains(&out, "kitsune-smp-ok");
    run_tap::assert_contains(&out, "kitsune-cpus=2");
    run_tap::assert_contains(&out, "kitsune-net-ok");
    run_tap::assert_contains(&out, "kitsune-net-tcp-ok");
}
