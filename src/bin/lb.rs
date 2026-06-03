#[path = "../fdpass.rs"]
mod fdpass;

use anyhow::{Context, Result};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::{
    net::SocketAddr,
    os::fd::{FromRawFd, RawFd},
};

fn main() -> Result<()> {
    let bind_addr = std::env::var("LB_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:9999".into());
    let backlog = std::env::var("LB_BACKLOG")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(4096);
    let accept_batch = std::env::var("LB_ACCEPT_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(64);
    let upstreams = std::env::var("FD_UPSTREAMS")
        .unwrap_or_else(|_| "/tmp/sock/api1.sock,/tmp/sock/api2.sock".into());
    let upstream_paths: Vec<String> = upstreams
        .split(',')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect();

    anyhow::ensure!(!upstream_paths.is_empty(), "FD_UPSTREAMS cannot be empty");

    let upstreams = connect_all_upstreams(&upstream_paths)?;
    let listener =
        bind_listener(&bind_addr, backlog).with_context(|| format!("bind {bind_addr}"))?;

    println!(
        "lb up addr={} backlog={} accept_batch={} upstreams={:?}",
        bind_addr, backlog, accept_batch, upstream_paths
    );

    #[cfg(target_os = "linux")]
    {
        run_linux(listener, upstreams, accept_batch)
    }

    #[cfg(not(target_os = "linux"))]
    {
        run_blocking(listener, upstreams)
    }
}

#[cfg(target_os = "linux")]
fn run_linux(listener: TcpListener, upstreams: Vec<UnixStream>, accept_batch: usize) -> Result<()> {
    let mut rr = 0usize;
    let lfd = listener.into_raw_fd();

    loop {
        let mut accepted = 0usize;
        while accepted < accept_batch {
            let cfd = unsafe {
                libc::accept4(
                    lfd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                )
            };
            if cfd < 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => break,
                    _ => return Err(err).context("accept4 failed"),
                }
            }

            accepted += 1;
            if let Err(err) = set_client_socket_options(cfd) {
                unsafe { libc::close(cfd) };
                return Err(err);
            }
            dispatch_client(&upstreams, &mut rr, cfd);
        }

        if accepted == 0 {
            wait_for_accept(lfd);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run_blocking(listener: TcpListener, upstreams: Vec<UnixStream>) -> Result<()> {
    let mut rr = 0usize;
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(conn) => conn,
            Err(_) => continue,
        };
        let client_fd = conn.into_raw_fd();
        if let Err(err) = set_client_socket_options(client_fd) {
            unsafe { libc::close(client_fd) };
            return Err(err);
        }
        dispatch_client(&upstreams, &mut rr, client_fd);
    }
    Ok(())
}

fn dispatch_client(upstreams: &[UnixStream], rr: &mut usize, client_fd: i32) {
    let first = *rr % upstreams.len();
    *rr = rr.wrapping_add(1);

    let mut delivered = false;
    for offset in 0..upstreams.len() {
        let idx = (first + offset) % upstreams.len();
        match fdpass::send_fd_nonblocking(upstreams[idx].as_raw_fd(), client_fd) {
            Ok(()) => {
                delivered = true;
                break;
            }
            Err(fdpass::SendFdError::WouldBlock) => continue,
            Err(fdpass::SendFdError::Io) => continue,
        }
    }

    if !delivered {
        let _ = fdpass::send_fd(upstreams[first].as_raw_fd(), client_fd);
    }

    unsafe { libc::close(client_fd) };
}

fn connect_all_upstreams(paths: &[String]) -> Result<Vec<UnixStream>> {
    let mut upstreams = Vec::with_capacity(paths.len());
    for path in paths {
        upstreams.push(connect_upstream(path)?);
    }
    Ok(upstreams)
}

fn connect_upstream(path: &str) -> Result<UnixStream> {
    let mut last_error = None;
    for _ in 0..200 {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                last_error = Some(err);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| std::io::Error::other("connect failed")))
        .with_context(|| format!("connect upstream socket {path}"))
}

fn bind_listener(bind_addr: &str, backlog: i32) -> Result<TcpListener> {
    #[cfg(target_os = "linux")]
    {
        bind_with_backlog_linux(bind_addr, backlog)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = backlog;
        TcpListener::bind(bind_addr).with_context(|| format!("bind {bind_addr}"))
    }
}

#[cfg(target_os = "linux")]
fn bind_with_backlog_linux(bind_addr: &str, backlog: i32) -> Result<TcpListener> {
    let addr: SocketAddr = bind_addr
        .parse()
        .with_context(|| format!("invalid LB_BIND_ADDR {bind_addr}"))?;

    let fd = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("socket() failed");
    }

    let one: i32 = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&one as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            (&one as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_DEFER_ACCEPT,
            (&one as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
    }

    let sockaddr = match addr {
        SocketAddr::V4(v4) => libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: v4.port().to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_be_bytes(v4.ip().octets()),
            },
            sin_zero: [0; 8],
        },
        SocketAddr::V6(_) => {
            unsafe { libc::close(fd) };
            anyhow::bail!("LB_BIND_ADDR IPv6 not supported by bind_with_backlog");
        }
    };

    let rc = unsafe {
        libc::bind(
            fd,
            (&sockaddr as *const libc::sockaddr_in).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_in>() as u32,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err).context("bind() failed");
    }

    if unsafe { libc::listen(fd, backlog) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err).context("listen() failed");
    }

    let listener = unsafe { TcpListener::from_raw_fd(fd as RawFd) };
    Ok(listener)
}

#[cfg(target_os = "linux")]
fn wait_for_accept(lfd: i32) {
    let mut pfd = libc::pollfd {
        fd: lfd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
        }
        break;
    }
}

#[cfg(target_os = "linux")]
fn set_client_socket_options(fd: i32) -> Result<()> {
    let one: i32 = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            (&one as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            (&one as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_client_socket_options(_fd: i32) -> Result<()> {
    Ok(())
}
