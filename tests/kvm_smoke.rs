//! KVM smoke tests.

fn require_kvm() {
    let path = std::path::Path::new("/dev/kvm");
    if !path.exists() {
        if std::env::var_os("KITSUNE_REQUIRE_KVM").is_some() {
            panic!("/dev/kvm is required but missing");
        }
        eprintln!("skipping: /dev/kvm not available");
        std::process::exit(0);
    }
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .is_err()
    {
        if std::env::var_os("KITSUNE_REQUIRE_KVM").is_some() {
            panic!("/dev/kvm exists but is not usable");
        }
        eprintln!("skipping: cannot open /dev/kvm");
        std::process::exit(0);
    }
}

#[test]
fn create_vm_and_debug_exit() {
    require_kvm();

    let config = kitsune::VmmConfig {
        mem_size: 4 * 1024 * 1024,
        num_vcpus: 1,
    };
    let mut vmm = kitsune::Vmm::new(&config).expect("Vmm::new");
    // Real mode: OUT 0x00 to port 0x501 (isa-debug-exit style), then optional HLT.
    // HLT alone is handled in-kernel when irqchip/PIT is enabled, so it is not a
    // reliable userspace stop signal.
    //   mov dx, 0x501
    //   xor al, al
    //   out dx, al
    let code = [0xba, 0x01, 0x05, 0x30, 0xc0, 0xee];
    vmm.load_flat_binary(&code, 0, 0).expect("load");
    vmm.run().expect("run to debug exit");
}
