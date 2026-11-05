//! The sandbox's seccomp filter: classic BPF assembled by hand, so no
//! libseccomp (rpctl sandbox.py `_seccomp_program`), with one refusal rpctl
//! lacks: `socket(AF_VSOCK)`, as Codex's filter (linux-sandbox/src/seccomp.rs;
//! kb d025). vsock reaches the hypervisor and other VMs, and no network
//! namespace confines it.
//!
//! Refused, with an errno so programs fail cleanly:
//! - `socket(AF_UNIX, ...)`, the only way to reach a named or abstract Unix
//!   socket (the X server, D-Bus), and `socket(AF_VSOCK, ...)`:
//!   EAFNOSUPPORT. `socketpair()` stays allowed (asyncio, libuv, pipes).
//! - `io_uring_setup`: io_uring can create sockets without `socket`.
//! - `unshare`, `clone(CLONE_NEWUSER)`: EPERM; `clone3`: ENOSYS, so glibc
//!   falls back to `clone`, whose flags the filter sees. Nested user
//!   namespaces are the kernel's largest attack surface.
//! - any syscall ABI other than x86_64's, x32 included, which would dodge
//!   the numbers above.
//!
//! x86_64 only; elsewhere [`super::available`] says so and run_shell asks.

/// `AUDIT_ARCH_X86_64`.
pub const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const X32_BIT: u32 = 0x4000_0000;
pub const NR_SOCKET: u32 = 41;
pub const NR_CLONE: u32 = 56;
pub const NR_UNSHARE: u32 = 272;
pub const NR_IO_URING_SETUP: u32 = 425;
pub const NR_CLONE3: u32 = 435;
pub const AF_UNIX: u32 = 1;
pub const AF_VSOCK: u32 = 40;
pub const CLONE_NEWUSER: u32 = 0x1000_0000;
pub const RET_ALLOW: u32 = 0x7FFF_0000;
pub const RET_ERRNO: u32 = 0x0005_0000;
pub const EPERM: u32 = 1;
pub const ENOSYS: u32 = 38;
pub const EAFNOSUPPORT: u32 = 97;

/// `BPF_LD | BPF_W | BPF_ABS`.
pub const LD_W_ABS: u16 = 0x20;
pub const JEQ_K: u16 = 0x15;
pub const JGE_K: u16 = 0x35;
pub const JSET_K: u16 = 0x45;
pub const RET_K: u16 = 0x06;
/// Offsets in `struct seccomp_data` (little endian): nr, arch, args[0]'s
/// low half.
pub const OFF_NR: u32 = 0;
pub const OFF_ARCH: u32 = 4;
pub const OFF_ARG0: u32 = 16;

/// One `struct sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

const fn stmt(code: u16, k: u32) -> Insn {
    Insn {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code, jt, jf, k }
}

const fn errno(e: u32) -> u32 {
    RET_ERRNO | e
}

/// The filter's instructions.
pub fn instructions() -> Vec<Insn> {
    vec![
        // [0-2] the native ABI only
        stmt(LD_W_ABS, OFF_ARCH),
        jump(JEQ_K, AUDIT_ARCH_X86_64, 1, 0),
        stmt(RET_K, errno(EPERM)),
        // [3-5] no x32
        stmt(LD_W_ABS, OFF_NR),
        jump(JGE_K, X32_BIT, 0, 1),
        stmt(RET_K, errno(EPERM)),
        // [6-11] socket: AF_UNIX and AF_VSOCK refused (rpctl: AF_UNIX only)
        jump(JEQ_K, NR_SOCKET, 0, 5),
        stmt(LD_W_ABS, OFF_ARG0),
        jump(JEQ_K, AF_UNIX, 1, 0),
        jump(JEQ_K, AF_VSOCK, 0, 1),
        stmt(RET_K, errno(EAFNOSUPPORT)),
        stmt(RET_K, RET_ALLOW),
        // not socket: the accumulator still holds nr
        jump(JEQ_K, NR_IO_URING_SETUP, 0, 1),
        stmt(RET_K, errno(EPERM)),
        jump(JEQ_K, NR_UNSHARE, 0, 1),
        stmt(RET_K, errno(EPERM)),
        jump(JEQ_K, NR_CLONE3, 0, 1),
        stmt(RET_K, errno(ENOSYS)),
        jump(JEQ_K, NR_CLONE, 0, 3),
        stmt(LD_W_ABS, OFF_ARG0),
        jump(JSET_K, CLONE_NEWUSER, 0, 1),
        stmt(RET_K, errno(EPERM)),
        stmt(RET_K, RET_ALLOW),
    ]
}

/// The filter as bwrap's `--seccomp` reads it: 8 bytes per instruction,
/// little endian.
pub fn program() -> Vec<u8> {
    encode(&instructions())
}

/// `insns` as bytes.
pub fn encode(insns: &[Insn]) -> Vec<u8> {
    let mut out = Vec::with_capacity(insns.len() * 8);
    for i in insns {
        out.extend_from_slice(&i.code.to_le_bytes());
        out.push(i.jt);
        out.push(i.jf);
        out.extend_from_slice(&i.k.to_le_bytes());
    }
    out
}

/// `bytes` as instructions; `None` unless whole instructions.
pub fn decode(bytes: &[u8]) -> Option<Vec<Insn>> {
    if !bytes.len().is_multiple_of(8) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(8)
            .map(|c| Insn {
                code: u16::from_le_bytes([c[0], c[1]]),
                jt: c[2],
                jf: c[3],
                k: u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            })
            .collect(),
    )
}

/// What `insns` returns for a syscall, run as the kernel runs a classic
/// BPF filter over the opcodes the filter uses; `None` for any other
/// opcode, a load the filter does not make, or a jump out of the program.
/// For tests: two programs compared over many inputs.
pub fn evaluate(insns: &[Insn], arch: u32, nr: u32, arg0: u32) -> Option<u32> {
    let mut acc = 0u32;
    let mut pc = 0usize;
    while let Some(i) = insns.get(pc) {
        pc += 1;
        let taken = match i.code {
            LD_W_ABS => {
                acc = match i.k {
                    OFF_NR => nr,
                    OFF_ARCH => arch,
                    OFF_ARG0 => arg0,
                    _ => return None,
                };
                continue;
            }
            RET_K => return Some(i.k),
            JEQ_K => acc == i.k,
            JGE_K => acc >= i.k,
            JSET_K => acc & i.k != 0,
            _ => return None,
        };
        pc += usize::from(if taken { i.jt } else { i.jf });
    }
    None
}

#[cfg(test)]
#[path = "seccomp_tests.rs"]
mod tests;
