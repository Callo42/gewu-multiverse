//! Live, per-spawn evidence that a run has no remaining descendants.

use std::fs;
use std::io::{self, IoSliceMut};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::Stdio;

use nix::errno::Errno;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};

const READY: &[u8] = b"multiverse supervisor v1";
const EMPTY: u8 = 1;
pub(crate) const CHANNEL_ENV: &str = "GEWU_SUPERVISOR_CHANNEL";

pub(crate) fn channel() -> io::Result<(UnixDatagram, Stdio)> {
    let (receiver, sender) = UnixDatagram::pair()?;
    rustix::net::sockopt::set_socket_passcred(&receiver, true)?;
    receiver.send(READY)?;
    Ok((receiver, Stdio::from(OwnedFd::from(sender))))
}

// The parent supplies this private endpoint as stdin only to the real supervisor.
// Restore the usual null stdin and keep the private endpoint close-on-exec.
fn become_subreaper() -> Option<UnixDatagram> {
    if std::env::var_os(CHANNEL_ENV).is_none() {
        return None;
    }
    let stdin = io::stdin();
    if rustix::net::sockopt::get_socket_type(&stdin).ok()? != rustix::net::SocketType::DGRAM {
        return None;
    }
    let socket = UnixDatagram::from(stdin.as_fd().try_clone_to_owned().ok()?);
    let mut message = [0; 32];
    let n = rustix::net::recv(&socket, &mut message, RecvFlags::DONTWAIT).ok()?;
    if &message[..n] != READY {
        return None;
    }
    nix::unistd::dup2_stdin(fs::File::open("/dev/null").ok()?).ok()?;
    nix::sys::prctl::set_child_subreaper(true).ok()?;
    Some(socket)
}

fn tree_is_empty() -> bool {
    // A background thread could still create children after the wait. Only the
    // final, single-threaded exit path is eligible for a positive receipt.
    let Ok(mut threads) = fs::read_dir("/proc/self/task") else {
        return false;
    };
    if !matches!(threads.next(), Some(Ok(_))) || threads.next().is_some() {
        return false;
    }
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG | WaitPidFlag::__WALL)) {
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) | Err(Errno::EINTR) => continue,
            Err(Errno::ECHILD) => return true,
            _ => return false,
        }
    }
}

/// Run the exact same worker entry point with optional live cleanup reporting.
pub fn execute(run_dir: &Path) -> crate::agent_run::RunEnd {
    let supervisor = become_subreaper();
    let end = crate::agent_run::execute(run_dir);
    if let Some(socket) = supervisor
        && tree_is_empty()
    {
        let _ = socket.send(&[EMPTY]);
    }
    end
}

pub(crate) fn received_empty_tree(socket: &UnixDatagram, pid: u32) -> bool {
    let mut message = [0; 2];
    let mut space = [0; rustix::cmsg_space!(ScmCredentials(1))];
    let mut credentials = RecvAncillaryBuffer::new(&mut space);
    let Ok(result) = rustix::net::recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut message)],
        &mut credentials,
        RecvFlags::DONTWAIT,
    ) else {
        return false;
    };
    result.bytes == 1
        && message[0] == EMPTY
        && credentials.drain().any(|item| {
            matches!(item, RecvAncillaryMessage::ScmCredentials(cred)
                if cred.pid.as_raw_nonzero().get() as u32 == pid)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_receipt_from_the_expected_pid_counts() {
        let (receiver, sender) = UnixDatagram::pair().unwrap();
        rustix::net::sockopt::set_socket_passcred(&receiver, true).unwrap();
        let pid = std::process::id();
        assert!(!received_empty_tree(&receiver, pid));
        sender.send(&[EMPTY]).unwrap();
        assert!(!received_empty_tree(&receiver, pid + 1));
        sender.send(&[EMPTY]).unwrap();
        assert!(received_empty_tree(&receiver, pid));
    }
}
