//! A proxied sandbox's network: the launcher that binds the proxy port
//! inside and the handover of that port to the client (rpctl sandbox.py
//! `LAUNCHER_ARGV`, `_LAUNCHER`, `_take_listener`).
//!
//! The launcher runs first in the sandbox, from the read-only /usr with
//! `python3 -I -S`, so nothing in the workspace or /tmp changes what it
//! does. It binds 127.0.0.1:3128 on the namespace's own loopback, sends the
//! listening socket over the socket pair whose end is its first argument,
//! waits for the client's go, closes both and only then execs the command,
//! which holds no descriptor of either. Python ignores SIGPIPE and SIGXFSZ
//! and exec would pass that on, so they are reset first. Kept in Python,
//! as rpctl's: the sandbox has python3 in /usr wherever rp's did, and a
//! launcher of viy's own would have to be bound in.

use std::io;
use std::io::Write as _;
use std::mem;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::PROXY_HOST;
use super::PROXY_PORT;

/// The interpreter the launcher runs with.
pub const LAUNCHER_ARGV: [&str; 4] = ["python3", "-I", "-S", "-c"];

/// rpctl's launcher, word for word but its messages, which name viy (flags F21).
pub const LAUNCHER: &str = r#"
import os, signal, socket, sys
fd, argv = int(sys.argv[1]), sys.argv[3:]
try:
    pair = socket.socket(fileno=fd)
    lis = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    lis.bind(('127.0.0.1', 3128))
    lis.listen(128)
    socket.send_fds(pair, [b"L"], [lis.fileno()])
    go = pair.recv(1)
    lis.close()
    pair.close()
except OSError as e:
    sys.stderr.write("viy: network launcher failed: %s\n" % e)
    sys.exit(125)
if go != b"G":
    sys.stderr.write("viy: the client did not take the proxy port\n")
    sys.exit(125)
for sig in (signal.SIGPIPE, signal.SIGXFSZ):
    signal.signal(sig, signal.SIG_DFL)
try:
    os.execvp(argv[0], argv)
except OSError as e:
    sys.stderr.write("viy: cannot run %s: %s\n" % (argv[0], e))
    sys.exit(127)
"#;

/// How long the launcher has to hand the port over.
pub const HANDOVER_TIMEOUT: Duration = Duration::from_secs(15);

/// What serves the proxy port a sandbox hands over: the egress proxy (WP
/// 4.8), or a test's stand-in.
pub trait Egress: Send + Sync {
    /// Starts serving `listener`, without blocking.
    fn serve(&self, listener: TcpListener);
    /// Stops: the listener and every connection close. Called at least
    /// once per command, possibly more, and possibly before a late
    /// [`Egress::serve`] (the caller cancelled mid-handover), which must
    /// then close too.
    fn close(&self);
}

/// How a handover failed, in rpctl's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSetupError(pub String);

/// An error's kind as a word, where rpctl gives Python's exception class.
fn kind(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => "TimedOut".into(),
        k => format!("{k:?}"),
    }
}

/// One message from `sock` with the descriptors it carries (at most 4,
/// close-on-exec).
fn recv_fds(sock: &UnixStream) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut buf = [0u8; 16];
    // SAFETY: CMSG_SPACE is a size computation.
    let space = unsafe { libc::CMSG_SPACE((4 * mem::size_of::<libc::c_int>()) as u32) } as usize;
    // u64s, so the control buffer is aligned for cmsghdr.
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: a zeroed msghdr is valid; its pointers are set below to
    // buffers that outlive the call.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space;
    // SAFETY: `msg` describes valid, writable buffers.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: walking the control messages the kernel just wrote, within
    // `msg_controllen`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<libc::c_int>();
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / mem::size_of::<libc::c_int>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((buf[..n.unsigned_abs()].to_vec(), fds))
}

fn sockopt(fd: &OwnedFd, name: libc::c_int) -> Option<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an int-sized option into an int.
    let r = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            name,
            (&raw mut value).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(value)
}

/// Whether `fd` is a listening TCP socket on 127.0.0.1:3128.
fn is_proxy_port(fd: &OwnedFd) -> bool {
    sockopt(fd, libc::SO_DOMAIN) == Some(libc::AF_INET)
        && sockopt(fd, libc::SO_TYPE) == Some(libc::SOCK_STREAM)
        && sockopt(fd, libc::SO_ACCEPTCONN) == Some(1)
}

/// Receives the sandbox's listening socket over `sock`, checks it, has
/// `proxy` serve it and tells the launcher to go on (rp's
/// `_take_listener`).
pub fn take_listener(sock: &mut UnixStream, proxy: &dyn Egress) -> Result<(), NetSetupError> {
    let fail = |s: String| Err(NetSetupError(s));
    let _ = sock.set_read_timeout(Some(HANDOVER_TIMEOUT));
    let (msg, mut fds) = match recv_fds(sock) {
        Ok(got) => got,
        Err(e) => return fail(format!("no proxy port from the sandbox ({})", kind(&e))),
    };
    // Any extra descriptor is closed as it drops.
    fds.truncate(1);
    let Some(fd) = fds.pop() else {
        return fail("the sandbox's network launcher did not start".into());
    };
    let wanted = SocketAddr::from((PROXY_HOST, PROXY_PORT));
    let ok = msg == b"L" && is_proxy_port(&fd);
    let listener = TcpListener::from(fd);
    if !ok || listener.local_addr().ok() != Some(wanted) {
        return fail("the sandbox handed over something that is not the proxy port".into());
    }
    proxy.serve(listener);
    if let Err(e) = sock.write_all(b"G") {
        return fail(format!(
            "the sandbox's network launcher went away ({})",
            kind(&e)
        ));
    }
    Ok(())
}
