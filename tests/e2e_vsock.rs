//! End-to-end virtio-vsock in both directions.

#[path = "support/guest.rs"]
mod guest;
#[path = "support/kvm.rs"]
mod kvm;
#[path = "support/run.rs"]
mod run;

use std::io::Read as _;
use std::io::Write as _;

const GUEST_PORT: &str = "5253";

#[test]
fn vsock_streams_both_directions() {
    kvm::require_kvm();
    guest::prepare_guest();
    let images = guest::guest_dir();
    let tmp = std::env::temp_dir().join(format!("kitsune-vsock-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let uds = tmp.join("v.sock");
    let peer_path = tmp.join("v.sock_5252");

    let listener = std::os::unix::net::UnixListener::bind(&peer_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || host_peer(listener));

    let uds_for_guest = uds.clone();
    let host_conn = std::thread::spawn(move || connect_guest(&uds_for_guest));

    let kernel = images.join("vmlinux");
    let initrd = images.join("initrd.img");
    let vsock_arg = format!("3:{}", uds.display());
    let (output, _) = run::run_until(
        &[
            "run",
            "--kernel",
            kernel.to_str().unwrap(),
            "--initrd",
            initrd.to_str().unwrap(),
            "--memory",
            "512",
            "--vsock",
            vsock_arg.as_str(),
            "--cmdline",
            "console=ttyS0 reboot=k panic=1 pci=off nomodule kitsune.vsock",
        ],
        std::time::Duration::from_secs(40),
        &[
            "kitsune-vsock-tx-ok",
            "kitsune-vsock-listen",
            "kitsune-vsock-rx-ok",
        ],
    );

    let peer_result = peer.join().expect("host peer thread");
    let host_result = host_conn.join().expect("host connect thread");
    let _ = std::fs::remove_dir_all(&tmp);

    run::assert_contains(&output, "kitsune-vsock-tx-ok");
    run::assert_contains(&output, "kitsune-vsock-listen");
    run::assert_contains(&output, "kitsune-vsock-rx-ok");
    assert!(
        !output.contains("kitsune-vsock-fail"),
        "guest vsock helper failed:\n{output}"
    );
    peer_result.expect("guest-to-host exchange");
    host_result.expect("host-to-guest exchange");
}

fn host_peer(listener: std::os::unix::net::UnixListener) -> std::io::Result<()> {
    let start = std::time::Instant::now();
    let (mut sock, _) = loop {
        if start.elapsed() > std::time::Duration::from_secs(30) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "guest did not connect to the host port",
            ));
        }
        match listener.accept() {
            Ok(pair) => break pair,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(err) => return Err(err),
        }
    };
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    sock.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    let got = read_until(&mut sock, "guest-to-host\n", 64)?;
    if !got.contains("guest-to-host\n") {
        return Err(std::io::Error::other(format!("peer payload: {got:?}")));
    }
    sock.write_all(b"host-ack\n")?;
    Ok(())
}

fn connect_guest(uds: &std::path::Path) -> std::io::Result<()> {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > std::time::Duration::from_secs(30) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "guest did not accept the host connection",
            ));
        }
        let mut sock = match std::os::unix::net::UnixStream::connect(uds) {
            Ok(sock) => sock,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        };
        sock.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        sock.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
        if sock
            .write_all(format!("CONNECT {GUEST_PORT}\n").as_bytes())
            .is_err()
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        let ok = match read_until(&mut sock, "\n", 64) {
            Ok(text) => text,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        };
        if !ok.starts_with("OK ") {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        sock.write_all(b"host-ping\n")?;
        sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let pong = read_until(&mut sock, "guest-pong\n", 64)?;
        if !pong.contains("guest-pong\n") {
            return Err(std::io::Error::other(format!("vsock payload: {pong:?}")));
        }
        return Ok(());
    }
}

fn read_until(
    sock: &mut std::os::unix::net::UnixStream,
    needle: &str,
    cap: usize,
) -> std::io::Result<String> {
    let mut got = String::new();
    let mut buf = [0u8; 64];
    while !got.contains(needle) {
        if got.len() > cap {
            return Err(std::io::Error::other(format!(
                "vsock reply too long: {got:?}"
            )));
        }
        match sock.read(&mut buf) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("eof before {needle:?}: {got:?}"),
                ));
            }
            Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(err);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(got)
}
