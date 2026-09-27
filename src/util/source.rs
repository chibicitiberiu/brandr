//! Input sources: a local file, or an image streamed over plain HTTP.
//!
//! HTTP images are never downloaded as a whole: the size comes from a `HEAD`
//! request, reads come from one streaming `GET`, and seeking (the rewind before
//! verification) drops the connection and reopens it with a `Range` header on
//! the next read. That keeps memory use to a few buffers no matter how large
//! the image is, which is the point on a 256MB netbooted machine.
//!
//! Network hiccups are retried: a failed connect, a stalled or dropped
//! connection reconnects with `Range: bytes=<pos>-` and carries on at the exact
//! byte it stopped, with exponential backoff. Answers that retrying can't fix
//! (404, a server that ignores Range) fail immediately.
//!
//! The client is a deliberately small HTTP/1.1 implementation on std's
//! `TcpStream`: it only ever talks to a static file server (nginx on the PXE
//! box), and owning the socket gives us a real per-read idle timeout, which is
//! what turns an unplugged cable into a retry instead of a 15 minute hang.

use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    time::Duration,
};

use tracing::{debug, info, warn};

/// Attempts per incident (a successful read resets the count).
const MAX_ATTEMPTS: u32 = 8;
/// Backoff between attempts: 1, 2, 4, 8, then capped at 15 seconds.
const MAX_BACKOFF: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// No data for this long counts as a hiccup and triggers a reconnect.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("brandr/", env!("CARGO_PKG_VERSION"));

/// Whether `path` is actually an `http://` URL rather than a filesystem path.
pub fn is_url(path: &Path) -> bool {
    path.to_str().is_some_and(|s| s.starts_with("http://"))
}

/// Open a path or URL for reading.
pub fn open_input(path: &Path) -> io::Result<InputSource> {
    if is_url(path) {
        Ok(InputSource::Http(HttpSource::open(url_of(path)?)?))
    } else {
        Ok(InputSource::File(File::open(path)?))
    }
}

/// Size in bytes of a path or URL.
pub fn input_size(path: &Path) -> io::Result<u64> {
    if is_url(path) {
        let url = Url::parse(url_of(path)?)?;
        with_retries(&url.raw, "HEAD", || head_size(&url))
    } else {
        Ok(File::open(path)?.metadata()?.len())
    }
}

/// GET a small document (the image catalog) into memory, with retries.
pub fn fetch_small(url: &str, limit: u64) -> io::Result<Vec<u8>> {
    let url = Url::parse(url)?;
    with_retries(&url.raw, "GET", || {
        let (status, headers, reader) = request(&url, "GET", None)?;
        if status != 200 {
            return Err(status_error(status));
        }
        let mut body = Vec::new();
        match content_length(&headers) {
            Some(len) if len > limit => {
                return Err(invalid_input(format!(
                    "{}: {len} bytes is too large",
                    url.raw
                )));
            }
            Some(len) => {
                reader.take(len).read_to_end(&mut body)?;
                if body.len() as u64 != len {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short body"));
                }
            }
            // no length: Connection: close means the body ends with the connection
            None => {
                reader.take(limit).read_to_end(&mut body)?;
            }
        }
        Ok(body)
    })
}

fn url_of(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| invalid_input("URL is not valid UTF-8".into()))
}

pub enum InputSource {
    File(File),
    Http(HttpSource),
}

impl Read for InputSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            InputSource::File(f) => f.read(buf),
            InputSource::Http(h) => h.read(buf),
        }
    }
}

impl Seek for InputSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            InputSource::File(f) => f.seek(pos),
            InputSource::Http(h) => h.seek(pos),
        }
    }
}

/// A seekable view of an HTTP resource that streams instead of buffering.
pub struct HttpSource {
    url: Url,
    size: u64,
    pos: u64,
    /// Open response body, positioned at `pos`
    body: Option<io::Take<BufReader<TcpStream>>>,
}

impl HttpSource {
    pub fn open(url: &str) -> io::Result<Self> {
        let url = Url::parse(url)?;
        let size = with_retries(&url.raw, "HEAD", || head_size(&url))?;
        info!(url = url.raw, size, "Opened HTTP source");
        Ok(Self {
            url,
            size,
            pos: 0,
            body: None,
        })
    }

    fn connect(&mut self) -> io::Result<()> {
        debug!(url = self.url.raw, pos = self.pos, "Starting HTTP GET");
        let range = (self.pos > 0).then(|| format!("Range: bytes={}-\r\n", self.pos));
        let (status, headers, reader) = request(&self.url, "GET", range.as_deref())?;
        match status {
            200 if self.pos == 0 => {}
            206 if self.pos > 0 => {}
            200 => {
                // A server that ignores Range would silently hand us byte 0 again
                return Err(io::Error::other(
                    "server did not honour the Range request".to_string(),
                ));
            }
            other => return Err(status_error(other)),
        }
        let len = content_length(&headers)
            .ok_or_else(|| io::Error::other("server sent no Content-Length".to_string()))?;
        if len != self.size - self.pos {
            return Err(io::Error::other(format!(
                "server returned {len} bytes from offset {}, expected {}",
                self.pos,
                self.size - self.pos
            )));
        }
        self.body = Some(reader.take(len));
        Ok(())
    }

    /// One read attempt: (re)connect if needed, then read from the stream.
    fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.body.is_none() {
            self.connect()?;
        }
        let n = self.body.as_mut().expect("connected above").read(buf)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("connection closed at byte {} of {}", self.pos, self.size),
            ));
        }
        Ok(n)
    }
}

impl Read for HttpSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let mut attempt = 1;
        loop {
            match self.try_read(buf) {
                Ok(n) => {
                    self.pos += n as u64;
                    return Ok(n);
                }
                Err(e) if is_retryable(&e) && attempt < MAX_ATTEMPTS => {
                    let wait = backoff(attempt);
                    warn!(url = self.url.raw, pos = self.pos, attempt, ?wait, error = %e,
                        "HTTP read failed, reconnecting at the same offset");
                    self.body = None;
                    std::thread::sleep(wait);
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Seek for HttpSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(off) => self.size.checked_add_signed(off),
            SeekFrom::Current(off) => self.pos.checked_add_signed(off),
        }
        .ok_or_else(|| invalid_input("seek before start".into()))?;
        if target != self.pos {
            self.body = None; // reopened with a Range header on the next read
            self.pos = target;
        }
        Ok(self.pos)
    }
}

/// The only URL shape we need: `http://host[:port]/path`.
#[derive(Debug, Clone)]
struct Url {
    raw: String,
    host: String,
    port: u16,
    path: String,
}

impl Url {
    fn parse(raw: &str) -> io::Result<Self> {
        let rest = raw
            .strip_prefix("http://")
            .ok_or_else(|| invalid_input(format!("{raw}: only http:// URLs are supported")))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse()
                    .map_err(|_| invalid_input(format!("{raw}: bad port")))?,
            ),
            None => (authority, 80),
        };
        if host.is_empty() || path.contains(' ') {
            return Err(invalid_input(format!(
                "{raw}: bad URL (spaces must be percent-encoded)"
            )));
        }
        Ok(Self {
            raw: raw.to_owned(),
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }
}

/// Status code, lowercased headers, and the connection positioned at the body.
type Response = (u16, Vec<(String, String)>, BufReader<TcpStream>);

/// Send one request on a fresh connection. Returns status, lowercased headers
/// and the reader positioned at the start of the body.
fn request(url: &Url, method: &str, extra_headers: Option<&str>) -> io::Result<Response> {
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "host did not resolve"))?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_write_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_nodelay(true)?;

    let host = if url.port == 80 {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let req = format!(
        "{method} {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {USER_AGENT}\r\n\
         Accept-Encoding: identity\r\nConnection: close\r\n{}\r\n",
        url.path,
        extra_headers.unwrap_or("")
    );
    (&stream).write_all(req.as_bytes())?;

    let mut reader = BufReader::with_capacity(1 << 16, stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other(format!("bad HTTP status line: {:?}", line.trim())))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed in headers",
            ));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    if header(&headers, "transfer-encoding").is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
        return Err(io::Error::other(
            "chunked/encoded responses are not supported".to_string(),
        ));
    }
    Ok((status, headers, reader))
}

fn head_size(url: &Url) -> io::Result<u64> {
    let (status, headers, _) = request(url, "HEAD", None)?;
    if status != 200 {
        return Err(status_error(status));
    }
    content_length(&headers)
        .ok_or_else(|| io::Error::other(format!("{}: server sent no Content-Length", url.raw)))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn content_length(headers: &[(String, String)]) -> Option<u64> {
    header(headers, "content-length").and_then(|v| v.parse().ok())
}

fn status_error(status: u16) -> io::Error {
    match status {
        404 => io::Error::new(io::ErrorKind::NotFound, "HTTP 404"),
        401 | 403 => io::Error::new(io::ErrorKind::PermissionDenied, format!("HTTP {status}")),
        400..=499 => invalid_input(format!("HTTP {status}")),
        // 5xx and anything odd: the server may recover, so retry
        _ => io::Error::other(format!("HTTP {status}")),
    }
}

/// Run `f`, retrying transient network errors with backoff.
fn with_retries<T>(url: &str, what: &str, mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 1;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if is_retryable(&e) && attempt < MAX_ATTEMPTS => {
                let wait = backoff(attempt);
                warn!(url, what, attempt, ?wait, error = %e, "HTTP request failed, retrying");
                std::thread::sleep(wait);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << (attempt - 1).min(4)).min(MAX_BACKOFF)
}

/// Network trouble is worth retrying; "the server said no" is not.
fn is_retryable(e: &io::Error) -> bool {
    let msg = e.to_string();
    !matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
    ) && !msg.contains("did not honour the Range")
        && !msg.contains("no Content-Length")
        && !msg.contains("not supported")
}

fn invalid_input(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = Url::parse("http://10.0.0.10:8069/iso/a%20b.iso").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("10.0.0.10", 8069, "/iso/a%20b.iso")
        );
        let u = Url::parse("http://server/x.img").unwrap();
        assert_eq!((u.port, u.path.as_str()), (80, "/x.img"));
        assert!(Url::parse("https://server/x").is_err());
        assert!(Url::parse("http://server/a b.iso").is_err());
    }

    #[test]
    fn backoff_is_capped() {
        let waits: Vec<u64> = (1..=8).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 15, 15, 15, 15]);
    }
}
