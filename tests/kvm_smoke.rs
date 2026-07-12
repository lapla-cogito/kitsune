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
fn create_vm_and_hlt() {
    require_kvm();

    let config = kitsune::VmmConfig {
        mem_size: 4 * 1024 * 1024,
        num_vcpus: 1,
    };
    let mut vmm = kitsune::Vmm::new(&config).expect("Vmm::new");
    // Real-mode single-byte program: HLT
    vmm.load_flat_binary(&[0xf4], 0, 0).expect("load");
    vmm.run().expect("run to hlt");
}
