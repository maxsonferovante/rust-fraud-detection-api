mod fdpass;
mod json;
mod models;
mod normalization;
mod search;

use crate::models::NormalizationConstants;
use crate::search::VectorStore;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::File;
#[cfg(not(target_os = "linux"))]
use std::io::{Read, Write};
#[cfg(not(target_os = "linux"))]
use std::net::TcpStream;
use std::os::fd::AsRawFd;
#[cfg(not(target_os = "linux"))]
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixListener;
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

const MAX_HTTP_BYTES: usize = 16 * 1024;
#[cfg(target_os = "linux")]
const MAX_EVENTS: usize = 1024;

struct AppState {
    vector_store: VectorStore,
    normalization_constants: NormalizationConstants,
    mcc_table: Vec<f32>,
    n_probes: usize,
    ready_response: Vec<u8>,
}

fn main() -> Result<()> {
    let sock_path =
        std::env::var("RINHA_SOCK_PATH").unwrap_or_else(|_| "/tmp/sock/api.sock".into());

    let (state, index_path) = load_state()?;
    let state = Arc::new(state);

    if let Some(parent) = Path::new(&sock_path).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket dir {}", parent.display()))?;
    }
    let _ = std::fs::remove_file(&sock_path);
    let listener =
        UnixListener::bind(&sock_path).with_context(|| format!("bind Unix socket {sock_path}"))?;

    println!(
        "api up sock={} n_probes={} index={} max_http_bytes={} io_mode={}",
        sock_path,
        state.n_probes,
        index_path,
        MAX_HTTP_BYTES,
        if cfg!(target_os = "linux") {
            "epoll"
        } else {
            "blocking"
        }
    );

    #[cfg(target_os = "linux")]
    {
        run_epoll(listener, state)
    }

    #[cfg(not(target_os = "linux"))]
    {
        run_blocking(listener, state)
    }
}

fn load_state() -> Result<(AppState, String)> {
    let n_probes = std::env::var("N_PROBES")
        .unwrap_or_else(|_| "192".to_string())
        .parse::<usize>()
        .unwrap_or(192);

    let norm_file = File::open("resources/normalization.json")?;
    let normalization_constants: NormalizationConstants = serde_json::from_reader(norm_file)?;

    let mcc_file = File::open("resources/mcc_risk.json")?;
    let mcc_json: HashMap<String, f32> = serde_json::from_reader(mcc_file)?;
    let mut mcc_table = vec![0.5f32; 10_000];
    for (key, val) in &mcc_json {
        if let Ok(idx) = key.parse::<usize>() {
            if idx < 10_000 {
                mcc_table[idx] = *val;
            }
        }
    }

    let index_path =
        std::env::var("RINHA_INDEX_PATH").unwrap_or_else(|_| "resources/specialist.bin".into());
    let vector_store = VectorStore::load(&index_path)?;

    let ready_response = READY_RESPONSE.as_bytes().to_vec();
    println!(
        "loaded index={} vectors={} clusters={} n_probes={} norm_fields={} mcc_entries={}",
        index_path,
        vector_store.len(),
        vector_store.n_clusters(),
        n_probes,
        7,
        mcc_json.len()
    );

    Ok((
        AppState {
            vector_store,
            normalization_constants,
            mcc_table,
            n_probes,
            ready_response,
        },
        index_path,
    ))
}

#[cfg(not(target_os = "linux"))]
fn run_blocking(listener: UnixListener, state: Arc<AppState>) -> Result<()> {
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(conn) => conn,
            Err(_) => continue,
        };
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            while let Ok(Some(fd)) = fdpass::recv_fd(conn.as_raw_fd()) {
                handle_fd_blocking(fd, &state);
            }
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn handle_fd_blocking(fd: i32, state: &AppState) {
    let mut stream = unsafe { TcpStream::from_raw_fd(fd) };
    let mut buf = [0u8; MAX_HTTP_BYTES];
    let mut filled = 0usize;

    let response_kind = match read_http_request_blocking(&mut stream, &mut buf, &mut filled) {
        Ok(()) if buf[..filled].starts_with(b"GET /ready ") => ResponseKind::Ready,
        Ok(()) if buf[..filled].starts_with(b"POST /fraud-score ") => {
            ResponseKind::Static(handle_fraud_score(&buf[..filled], state).as_bytes())
        }
        _ => ResponseKind::Static(BAD_REQUEST_RESPONSE.as_bytes()),
    };

    let _ = match response_kind {
        ResponseKind::Ready => stream.write_all(&state.ready_response),
        ResponseKind::Static(bytes) => stream.write_all(bytes),
    };
}

#[cfg(not(target_os = "linux"))]
fn read_http_request_blocking(
    stream: &mut TcpStream,
    buf: &mut [u8],
    filled: &mut usize,
) -> Result<()> {
    let mut header_end = None;
    let mut content_length = 0usize;

    loop {
        let read = stream.read(&mut buf[*filled..])?;
        if read == 0 {
            break;
        }
        *filled += read;

        if header_end.is_none() {
            if let Some(pos) = find_header_end(&buf[..*filled]) {
                header_end = Some(pos);
                content_length = parse_content_length(&buf[..pos]).unwrap_or(0);
            }
        }

        if let Some(pos) = header_end {
            if *filled >= pos + 4 + content_length {
                return Ok(());
            }
        }

        if *filled == buf.len() {
            anyhow::bail!("request exceeded fixed buffer");
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn run_epoll(listener: UnixListener, state: Arc<AppState>) -> Result<()> {
    set_nonblocking(listener.as_raw_fd())?;
    let epfd = unsafe { libc::epoll_create1(0) };
    if epfd < 0 {
        return Err(std::io::Error::last_os_error()).context("epoll_create1 failed");
    }

    let mut event = libc::epoll_event {
        events: (libc::EPOLLIN | libc::EPOLLET) as u32,
        u64: listener.as_raw_fd() as u64,
    };
    if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, listener.as_raw_fd(), &mut event) } != 0
    {
        return Err(std::io::Error::last_os_error()).context("epoll_ctl add listener failed");
    }

    let mut unix_conns: HashMap<i32, UnixStream> = HashMap::new();
    let mut clients: Vec<Option<ClientState>> = Vec::new();
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; MAX_EVENTS];

    loop {
        let n = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), events.len() as i32, -1) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("epoll_wait failed");
        }

        for idx in 0..(n as usize) {
            let ev = events[idx];
            let fd = ev.u64 as i32;
            let flags = ev.events as i32;

            if fd == listener.as_raw_fd() {
                loop {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            set_nonblocking(conn.as_raw_fd())?;
                            let conn_fd = conn.as_raw_fd();
                            unix_conns.insert(conn_fd, conn);
                            let mut event = libc::epoll_event {
                                events: (libc::EPOLLIN | libc::EPOLLET | libc::EPOLLONESHOT) as u32,
                                u64: conn_fd as u64,
                            };
                            if unsafe {
                                libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, conn_fd, &mut event)
                            } != 0
                            {
                                return Err(std::io::Error::last_os_error())
                                    .context("epoll_ctl add unix conn failed");
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(err) => return Err(err).context("accept on unix listener failed"),
                    }
                }
                continue;
            }

            if unix_conns.contains_key(&fd) {
                drain_fd_messages(epfd, fd, &mut unix_conns, &mut clients)?;
                let mut event = libc::epoll_event {
                    events: (libc::EPOLLIN | libc::EPOLLET | libc::EPOLLONESHOT) as u32,
                    u64: fd as u64,
                };
                let _ = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, fd, &mut event) };
                continue;
            }

            if let Some(client) = clients.get_mut(fd as usize).and_then(|v| v.as_mut()) {
                if (flags & (libc::EPOLLHUP | libc::EPOLLERR)) != 0 {
                    client.close_requested = true;
                } else {
                    if (flags & libc::EPOLLIN) != 0 && !client.response_ready {
                        client.read_into_buffer()?;
                        if client.try_finalize_request() {
                            client.prepare_response(&state)?;
                            client.arm_write(epfd)?;
                        }
                        client.rearm_read(epfd)?;
                    }
                    if (flags & libc::EPOLLOUT) != 0 && client.response_ready {
                        client.flush_response(&state)?;
                        if client.write_done {
                            client.close_requested = true;
                        } else {
                            client.rearm_write(epfd)?;
                        }
                    }
                }

                if client.close_requested {
                    let _ = unsafe {
                        libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut())
                    };
                    unsafe { libc::close(fd) };
                    if let Some(slot) = clients.get_mut(fd as usize) {
                        *slot = None;
                    }
                }
            }
        }
    }
}

fn handle_fraud_score(buf: &[u8], state: &AppState) -> &'static str {
    let Some(header_end) = find_header_end(buf) else {
        return BAD_REQUEST_RESPONSE;
    };
    handle_fraud_score_at(buf, header_end, state)
}

fn handle_fraud_score_at(buf: &[u8], header_end: usize, state: &AppState) -> &'static str {
    let body_start = header_end + 4;
    let payload = match json::parse_transaction(&buf[body_start..]) {
        Some(payload) => payload,
        None => return BAD_REQUEST_RESPONSE,
    };

    let vector = normalization::normalize_parsed_i16(
        &payload,
        &state.normalization_constants,
        &state.mcc_table,
    );
    let frauds = state
        .vector_store
        .fraud_count_nearest_i16(&vector, state.n_probes);

    FRAUD_RESPONSES[frauds.min(5)]
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(target_os = "linux")]
fn find_header_end_from(buf: &[u8], start: usize) -> Option<usize> {
    if buf.len() < 4 || start >= buf.len() {
        return None;
    }
    let mut i = start;
    while i + 3 < buf.len() {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' && buf[i + 2] == b'\r' && buf[i + 3] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    parse_content_length_fast(headers)
}

fn parse_content_length_fast(headers: &[u8]) -> Option<usize> {
    // Fast path: scan for "content-length:" case-insensitive and parse decimal digits.
    let needle = b"content-length:";
    let mut i = 0usize;
    while i + needle.len() <= headers.len() {
        if ascii_eq_ignore_case(&headers[i..i + needle.len()], needle) {
            let mut j = i + needle.len();
            while j < headers.len() && headers[j].is_ascii_whitespace() {
                j += 1;
            }
            let mut value: usize = 0;
            let mut any = false;
            while j < headers.len() {
                let b = headers[j];
                if !b.is_ascii_digit() {
                    break;
                }
                any = true;
                value = value.saturating_mul(10).saturating_add((b - b'0') as usize);
                j += 1;
            }
            return any.then_some(value);
        }
        i += 1;
    }
    None
}

#[inline(always)]
fn ascii_eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for idx in 0..a.len() {
        if a[idx].to_ascii_lowercase() != b[idx] {
            return false;
        }
    }
    true
}

const READY_RESPONSE: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 24\r\nConnection: close\r\n\r\n{\"ok\":true,\"role\":\"api\"}";
const BAD_REQUEST_RESPONSE: &str =
    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

const FRAUD_RESPONSES: [&str; 6] = [
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: close\r\n\r\n{\"approved\":true,\"fraud_score\":0.0}",
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: close\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}",
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: close\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}",
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: close\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}",
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: close\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}",
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: close\r\n\r\n{\"approved\":false,\"fraud_score\":1.0}",
];

#[derive(Clone, Copy)]
enum ResponseKind {
    Ready,
    Static(&'static [u8]),
}

#[cfg(target_os = "linux")]
struct ClientState {
    fd: i32,
    buf: [u8; MAX_HTTP_BYTES],
    filled: usize,
    header_end: Option<usize>,
    header_search_pos: usize,
    content_length: usize,
    response_kind: ResponseKind,
    write_pos: usize,
    response_ready: bool,
    write_done: bool,
    close_requested: bool,
}

#[cfg(target_os = "linux")]
impl ClientState {
    fn new(fd: i32) -> Self {
        Self {
            fd,
            buf: [0u8; MAX_HTTP_BYTES],
            filled: 0,
            header_end: None,
            header_search_pos: 0,
            content_length: 0,
            response_kind: ResponseKind::Static(BAD_REQUEST_RESPONSE.as_bytes()),
            write_pos: 0,
            response_ready: false,
            write_done: false,
            close_requested: false,
        }
    }

    fn read_into_buffer(&mut self) -> Result<()> {
        loop {
            if self.filled == self.buf.len() {
                anyhow::bail!("request exceeded fixed buffer");
            }
            let read = unsafe {
                libc::read(
                    self.fd,
                    self.buf[self.filled..].as_mut_ptr().cast(),
                    self.buf.len() - self.filled,
                )
            };
            if read == 0 {
                self.close_requested = true;
                return Ok(());
            }
            if read < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(err).context("read client socket failed");
            }
            self.filled += read as usize;
        }
    }

    fn try_finalize_request(&mut self) -> bool {
        if self.header_end.is_none() {
            if let Some(pos) =
                find_header_end_from(&self.buf[..self.filled], self.header_search_pos)
            {
                self.header_end = Some(pos);
                self.content_length = parse_content_length_fast(&self.buf[..pos]).unwrap_or(0);
            } else {
                self.header_search_pos = self.filled.saturating_sub(3);
            }
        }
        if let Some(pos) = self.header_end {
            self.filled >= pos + 4 + self.content_length
        } else {
            false
        }
    }

    fn prepare_response(&mut self, state: &AppState) -> Result<()> {
        let data = &self.buf[..self.filled];
        self.response_kind = if data.starts_with(b"GET /ready ") {
            ResponseKind::Ready
        } else if data.starts_with(b"POST /fraud-score ") {
            ResponseKind::Static(match self.header_end {
                Some(header_end) => handle_fraud_score_at(data, header_end, state).as_bytes(),
                None => BAD_REQUEST_RESPONSE.as_bytes(),
            })
        } else {
            ResponseKind::Static(BAD_REQUEST_RESPONSE.as_bytes())
        };
        self.response_ready = true;
        Ok(())
    }

    fn arm_write(&mut self, epfd: i32) -> Result<()> {
        let mut event = libc::epoll_event {
            events: (libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLONESHOT) as u32,
            u64: self.fd as u64,
        };
        if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, self.fd, &mut event) } != 0 {
            return Err(std::io::Error::last_os_error()).context("epoll_ctl mod client failed");
        }
        Ok(())
    }

    fn rearm_read(&mut self, epfd: i32) -> Result<()> {
        if self.response_ready {
            return Ok(());
        }
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLET | libc::EPOLLONESHOT) as u32,
            u64: self.fd as u64,
        };
        if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, self.fd, &mut event) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("epoll_ctl mod client read failed");
        }
        Ok(())
    }

    fn rearm_write(&mut self, epfd: i32) -> Result<()> {
        if !self.response_ready || self.write_done {
            return Ok(());
        }
        self.arm_write(epfd)
    }

    fn flush_response(&mut self, state: &AppState) -> Result<()> {
        let bytes: &[u8] = match self.response_kind {
            ResponseKind::Ready => &state.ready_response,
            ResponseKind::Static(b) => b,
        };
        while self.write_pos < bytes.len() {
            let written = unsafe {
                libc::write(
                    self.fd,
                    bytes[self.write_pos..].as_ptr().cast(),
                    bytes.len() - self.write_pos,
                )
            };
            if written < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(err).context("write client socket failed");
            }
            self.write_pos += written as usize;
        }
        self.write_done = true;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn drain_fd_messages(
    epfd: i32,
    unix_fd: i32,
    unix_conns: &mut HashMap<i32, UnixStream>,
    clients: &mut Vec<Option<ClientState>>,
) -> Result<()> {
    let Some(conn) = unix_conns.get(&unix_fd) else {
        return Ok(());
    };
    loop {
        match fdpass::recv_fd(conn.as_raw_fd()) {
            Ok(Some(client_fd)) => {
                set_nonblocking(client_fd)?;
                let mut event = libc::epoll_event {
                    events: (libc::EPOLLIN | libc::EPOLLET | libc::EPOLLONESHOT) as u32,
                    u64: client_fd as u64,
                };
                if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, client_fd, &mut event) } != 0
                {
                    unsafe { libc::close(client_fd) };
                    continue;
                }
                let idx = client_fd as usize;
                if clients.len() <= idx {
                    clients.resize_with(idx + 1, || None);
                }
                clients[idx] = Some(ClientState::new(client_fd));
            }
            Ok(None) => {
                // Upstream closed
                let _ = unsafe {
                    libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, unix_fd, std::ptr::null_mut())
                };
                unix_conns.remove(&unix_fd);
                unsafe { libc::close(unix_fd) };
                break;
            }
            Err(err) => {
                let io_err = err
                    .downcast_ref::<std::io::Error>()
                    .map(|e| e.kind())
                    .unwrap_or(std::io::ErrorKind::Other);
                if io_err == std::io::ErrorKind::WouldBlock {
                    break;
                }
                // Drop connection on protocol errors
                let _ = unsafe {
                    libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, unix_fd, std::ptr::null_mut())
                };
                unix_conns.remove(&unix_fd);
                unsafe { libc::close(unix_fd) };
                break;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFL) failed");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_SETFL,O_NONBLOCK) failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_case_insensitive() {
        assert_eq!(
            parse_content_length(b"POST /x HTTP/1.1\r\ncontent-length: 42\r\n"),
            Some(42)
        );
    }

    #[test]
    fn finds_header_end() {
        assert_eq!(find_header_end(b"GET /ready HTTP/1.1\r\n\r\n"), Some(19));
    }
}
