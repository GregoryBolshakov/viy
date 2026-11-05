use pretty_assertions::assert_eq;

use super::*;

fn rpctl() -> Vec<Insn> {
    let fixture = viy_testkit::fixtures::load("sandbox/seccomp").unwrap();
    let hex = fixture.case("program").unwrap()["hex"].as_str().unwrap();
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    decode(&bytes).unwrap()
}

/// Inputs worth asking a filter about: every syscall number it names and
/// their neighbours, x32's, other ABIs, and first arguments around every
/// constant it compares with.
fn inputs() -> Vec<(u32, u32, u32)> {
    let nrs = [
        0,
        1,
        NR_SOCKET - 1,
        NR_SOCKET,
        NR_SOCKET + 1,
        53, // socketpair
        NR_CLONE,
        57, // fork
        NR_UNSHARE,
        NR_IO_URING_SETUP,
        426,
        NR_CLONE3,
        X32_BIT | NR_SOCKET,
        X32_BIT,
        u32::MAX,
    ];
    let args = [
        0,
        AF_UNIX,
        2,  // AF_INET
        10, // AF_INET6
        16, // AF_NETLINK
        17, // AF_PACKET
        AF_VSOCK - 1,
        AF_VSOCK,
        AF_VSOCK + 1,
        CLONE_NEWUSER,
        CLONE_NEWUSER | 0x0001_0000,
        0x0001_1200, // what fork's clone passes
        u32::MAX,
    ];
    let arches = [
        AUDIT_ARCH_X86_64,
        0x4000_0003, /* i386 */
        0xC000_00B7,
        0,
    ];
    let mut out = Vec::new();
    for &arch in &arches {
        for &nr in &nrs {
            for &arg in &args {
                out.push((arch, nr, arg));
            }
        }
    }
    out
}

#[test]
fn equals_rpctls_filter_but_for_vsock_sockets() {
    let rp = rpctl();
    let viy = instructions();
    let mut checked = 0;
    for (arch, nr, arg0) in inputs() {
        let theirs = evaluate(&rp, arch, nr, arg0).expect("rpctl's program runs to a return");
        let ours = evaluate(&viy, arch, nr, arg0).expect("viy's program runs to a return");
        let vsock = arch == AUDIT_ARCH_X86_64 && nr == NR_SOCKET && arg0 == AF_VSOCK;
        let want = if vsock {
            RET_ERRNO | EAFNOSUPPORT
        } else {
            theirs
        };
        assert_eq!(ours, want, "arch {arch:#x} nr {nr} arg0 {arg0:#x}");
        checked += 1;
    }
    assert_eq!(checked, 4 * 15 * 13);
    // rpctl lets vsock through, which is the one difference.
    assert_eq!(
        evaluate(&rp, AUDIT_ARCH_X86_64, NR_SOCKET, AF_VSOCK),
        Some(RET_ALLOW)
    );
}

#[test]
fn the_decisions_rpctl_documents() {
    let p = instructions();
    let x86 = |nr, arg| evaluate(&p, AUDIT_ARCH_X86_64, nr, arg).unwrap();
    assert_eq!(x86(NR_SOCKET, AF_UNIX), RET_ERRNO | EAFNOSUPPORT);
    assert_eq!(x86(NR_SOCKET, AF_VSOCK), RET_ERRNO | EAFNOSUPPORT);
    assert_eq!(x86(NR_SOCKET, 2), RET_ALLOW);
    assert_eq!(x86(53, AF_UNIX), RET_ALLOW, "socketpair stays");
    assert_eq!(x86(NR_IO_URING_SETUP, 0), RET_ERRNO | EPERM);
    assert_eq!(x86(NR_UNSHARE, 0), RET_ERRNO | EPERM);
    assert_eq!(x86(NR_CLONE3, 0), RET_ERRNO | ENOSYS);
    assert_eq!(x86(NR_CLONE, CLONE_NEWUSER), RET_ERRNO | EPERM);
    assert_eq!(x86(NR_CLONE, 0x0001_1200), RET_ALLOW);
    assert_eq!(x86(X32_BIT | NR_SOCKET, 2), RET_ERRNO | EPERM);
    assert_eq!(
        evaluate(&p, 0x4000_0003, 0, 0),
        Some(RET_ERRNO | EPERM),
        "i386"
    );
}

#[test]
fn bytes_round_trip() {
    let bytes = program();
    assert_eq!(bytes.len(), instructions().len() * 8);
    assert_eq!(decode(&bytes), Some(instructions()));
    assert_eq!(decode(&bytes[..9]), None);
    // rpctl's first instruction, byte for byte: ld [4].
    assert_eq!(&bytes[..8], &[0x20, 0, 0, 0, 4, 0, 0, 0]);
}

#[test]
fn the_launcher_is_rpctls() {
    let fixture = viy_testkit::fixtures::load("sandbox/seccomp").unwrap();
    let case = fixture.case("launcher").unwrap();
    assert_eq!(case["code"], super::super::launch::LAUNCHER);
    let argv: Vec<&str> = case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(argv, super::super::launch::LAUNCHER_ARGV);
    assert_eq!(
        case["timeout"].as_f64(),
        Some(super::super::launch::HANDOVER_TIMEOUT.as_secs_f64())
    );
}
