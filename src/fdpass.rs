use anyhow::{bail, Context};
use std::io;
use std::mem;
use std::os::fd::RawFd;

#[allow(dead_code)]
pub fn send_fd(sock_fd: RawFd, fd_to_send: RawFd) -> anyhow::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };

    let mut control =
        vec![0u8; unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            bail!("failed to allocate fd-passing control message");
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as _;
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        *data = fd_to_send;

        let sent = libc::sendmsg(sock_fd, &msg, libc::MSG_NOSIGNAL);
        if sent < 0 {
            return Err(io::Error::last_os_error()).context("sendmsg(SCM_RIGHTS) failed");
        }
    }

    Ok(())
}

#[allow(dead_code)]
pub fn recv_fd(sock_fd: RawFd) -> anyhow::Result<Option<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };

    let mut control =
        vec![0u8; unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;

    let received = unsafe { libc::recvmsg(sock_fd, &mut msg, 0) };
    if received == 0 {
        return Ok(None);
    }
    if received < 0 {
        return Err(io::Error::last_os_error()).context("recvmsg(SCM_RIGHTS) failed");
    }

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            bail!("received Unix message without a file descriptor");
        }
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        Ok(Some(*data))
    }
}
