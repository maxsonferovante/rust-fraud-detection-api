#[path = "../fdpass.rs"]
mod fdpass;

use anyhow::{Context, Result};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
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
    let workers = std::env::var("LB_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    let upstreams = std::env::var("FD_UPSTREAMS")
        .unwrap_or_else(|_| "/tmp/sock/api1.sock,/tmp/sock/api2.sock".into());
    let upstream_paths: Vec<String> = upstreams
        .split(',')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect();

    anyhow::ensure!(!upstream_paths.is_empty(), "FD_UPSTREAMS cannot be empty");

    let upstream_paths = Arc::new(upstream_paths);
    let rr = Arc::new(AtomicUsize::new(0));

    let listener =
        bind_listener(&bind_addr, backlog).with_context(|| format!("bind {bind_addr}"))?;
    let mut handles = Vec::with_capacity(workers);

    println!(
        "lb up addr={} workers={} backlog={} upstreams={:?}",
        bind_addr, workers, backlog, *upstream_paths
    );

    let mut senders = Vec::with_capacity(workers);
    for worker_id in 0..workers {
        let upstream_paths = Arc::clone(&upstream_paths);
        let rr = Arc::clone(&rr);
        let (tx, rx) = mpsc::channel::<i32>();
        senders.push(tx);
        handles.push(thread::spawn(move || {
            let upstreams = connect_all_upstreams(&upstream_paths).ok();
            if upstreams.is_none() {
                return;
            }
            let upstreams = upstreams.unwrap();
            worker_loop(worker_id, upstreams, rr, rx);
        }));
    }

    accept_loop(listener, senders);

    for handle in handles {
        let _ = handle.join();
    }

    Ok(())
}

fn accept_loop(listener: TcpListener, senders: Vec<mpsc::Sender<i32>>) {
    let mut next = 0usize;
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) => continue,
        };

        let _ = stream.set_nodelay(true);
        let client_fd = stream.into_raw_fd();

        let idx = next % senders.len();
        next = next.wrapping_add(1);
        if senders[idx].send(client_fd).is_err() {
            unsafe { libc::close(client_fd) };
        }
    }
}

fn worker_loop(
    _worker_id: usize,
    upstreams: Vec<UnixStream>,
    rr: Arc<AtomicUsize>,
    rx: mpsc::Receiver<i32>,
) {
    for client_fd in rx {
        let upstream_idx = rr.fetch_add(1, Ordering::Relaxed) % upstreams.len();
        if fdpass::send_fd(upstreams[upstream_idx].as_raw_fd(), client_fd).is_err() {
            unsafe { libc::close(client_fd) };
            continue;
        }
        unsafe { libc::close(client_fd) };
    }
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

    Err(last_error
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "connect failed")))
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

    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
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
