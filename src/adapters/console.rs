//! The console socket a session's pty master arrives on.
//!
//! The runtime allocates the pty inside the sandbox, from the sandbox's own
//! `/dev/pts`, and hands it over as an open file descriptor on a Unix socket.
//! Only hort listens on that socket, and only hort receives from it, so the
//! master never becomes a name anything else on the host can open.
//!
//! Receiving a descriptor is the one place hort turns a message from another
//! process into a file it will read and write, so a message that carries no
//! descriptor has to be an error rather than a number: read as one, whatever
//! integer happened to be in the buffer names a file hort already had open, and
//! the relay would then copy the user's own terminal into itself.

use std::io::{self, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::ptr;

/// How much of the message body is read. The runtime names the device it sent in
/// the payload and nothing here reads that name, so this only has to be big
/// enough that the body is taken along with the descriptor riding on it.
const PAYLOAD_BYTES: usize = 64;
/// Room for the control message, counted in words rather than bytes so the
/// buffer lands on the alignment a control-message header is read at.
const CONTROL_WORDS: usize = 4;

/// Take the file descriptor a peer sent over `socket`.
pub fn receive_descriptor(socket: &UnixStream) -> Result<OwnedFd, String> {
    let mut payload = [0u8; PAYLOAD_BYTES];
    let mut control = [0u64; CONTROL_WORDS];
    let mut body = IoSliceMut::new(&mut payload);
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = ptr::addr_of_mut!(body).cast();
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&control) as _;

    if unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, 0) } == -1 {
        return Err(format!("receiving a descriptor: {}", io::Error::last_os_error()));
    }

    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null() {
        return Err("the console message carried no descriptor".to_string());
    }
    let (level, kind) = unsafe { ((*header).cmsg_level, (*header).cmsg_type) };
    if (level, kind) != (libc::SOL_SOCKET, libc::SCM_RIGHTS) {
        return Err("the console message carried something other than a descriptor".to_string());
    }

    let descriptor = unsafe { ptr::read_unaligned(libc::CMSG_DATA(header).cast::<libc::c_int>()) };
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::IoSlice;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
    use std::os::unix::fs::MetadataExt;
    use std::ptr;

    use tempfile::NamedTempFile;

    /// Send one descriptor the way the runtime sends the pty master: a one-byte
    /// payload carrying an `SCM_RIGHTS` control message. Written here because
    /// hort only ever receives, and production code that exists to be tested is
    /// worse than a test that performs the effect itself.
    fn send_descriptor(socket: &UnixStream, descriptor: BorrowedFd<'_>) {
        let payload = [0u8; 1];
        let mut buffer = [0u8; 32];
        let mut iov = IoSlice::new(&payload);
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = ptr::addr_of_mut!(iov).cast();
        message.msg_iovlen = 1;
        message.msg_control = buffer.as_mut_ptr().cast();
        message.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as _;

        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as _;
            ptr::write(libc::CMSG_DATA(header).cast::<libc::c_int>(), descriptor.as_raw_fd());
            assert_ne!(libc::sendmsg(socket.as_raw_fd(), &message, 0), -1, "sending a descriptor");
        }
    }

    #[test]
    fn a_received_descriptor_refers_to_the_same_file_the_sender_passed() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let passed = NamedTempFile::new().unwrap();
        send_descriptor(&sender, passed.as_file().as_fd());

        let received = receive_descriptor(&receiver).unwrap();

        // A descriptor number means nothing across a process boundary, so the
        // only proof that the right file arrived is that it is the same file.
        let arrived = std::fs::File::from(received).metadata().unwrap();
        let sent = passed.as_file().metadata().unwrap();
        assert_eq!((arrived.dev(), arrived.ino()), (sent.dev(), sent.ino()));
    }

    #[test]
    fn a_console_message_without_a_descriptor_is_an_error() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        std::io::Write::write_all(&mut &sender, b"x").unwrap();

        let received = receive_descriptor(&receiver);

        // The buffer a descriptor would have been read out of holds whatever was
        // there before. Trusting it hands the relay a file hort already had open,
        // most often the terminal it is sitting on.
        assert!(received.is_err());
    }
}
