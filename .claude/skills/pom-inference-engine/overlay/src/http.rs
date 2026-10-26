//! Just enough HTTP/1.1 for loopback: the bridge's own server (one request
//! per connection) and a streaming client to the engine's server. Std only,
//! so the plugin needs no network stack of its own and never speaks to
//! anything beyond 127.0.0.1.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The largest request body the bridge reads (the POM sends at most 32 MiB).
pub const MAX_BODY: usize = 32 << 20;

pub struct Request {
    pub method: String,
    /// The path without the query.
    pub path: String,
    /// Decoded query parameters; the session routes read them (feature L4).
    #[allow(dead_code)]
    pub query: BTreeMap<String, String>,
    /// Lowercase names.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

fn decode_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&value[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_head<R: BufRead>(reader: &mut R) -> std::io::Result<Vec<String>> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let line = line.trim_end_matches(['\r', '\n']).to_owned();
        if line.is_empty() {
            return Ok(lines);
        }
        if lines.len() > 100 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        lines.push(line);
    }
}

fn parse_headers(lines: &[String]) -> BTreeMap<String, String> {
    lines
        .iter()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

/// Reads one request from a connection the bridge accepted.
pub fn read_request(stream: &TcpStream) -> std::io::Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream);
    let head = read_head(&mut reader)?;
    let first = head.first().ok_or(std::io::ErrorKind::InvalidData)?;
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode_component(key), decode_component(value))
        })
        .collect();
    let headers = parse_headers(&head[1..]);
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Request {
        method,
        path: path.trim_end_matches('/').to_owned(),
        query,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// A whole JSON answer.
pub fn respond_json(
    mut stream: &TcpStream,
    status: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        reason(status),
        body.len()
    )?;
    stream.flush()
}

/// A streamed answer (`application/x-ndjson`, chunked): `line` sends one
/// JSON object per line, `finish` ends the body.
pub struct NdjsonWriter<'a> {
    stream: &'a TcpStream,
}

impl<'a> NdjsonWriter<'a> {
    pub fn start(mut stream: &'a TcpStream) -> std::io::Result<Self> {
        stream.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        )?;
        stream.flush()?;
        Ok(Self { stream })
    }

    pub fn line(&mut self, value: &serde_json::Value) -> std::io::Result<()> {
        let mut text = value.to_string();
        text.push('\n');
        write!(self.stream, "{:x}\r\n{text}\r\n", text.len())?;
        self.stream.flush()
    }

    pub fn finish(self) -> std::io::Result<()> {
        let mut stream = self.stream;
        stream.write_all(b"0\r\n\r\n")?;
        stream.flush()
    }
}

/// The body of a response from the engine, chunked or not.
pub struct Body {
    reader: BufReader<TcpStream>,
    chunked: bool,
    /// Bytes left in the current chunk, or in the whole body.
    left: Option<usize>,
    done: bool,
}

impl Read for Body {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buffer.is_empty() {
            return Ok(0);
        }
        if self.chunked && self.left.is_none_or(|left| left == 0) {
            if self.left == Some(0) {
                let mut crlf = String::new();
                self.reader.read_line(&mut crlf)?;
            }
            let mut size = String::new();
            self.reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.left = Some(size);
        }
        let limit = match self.left {
            Some(left) => buffer.len().min(left),
            None => buffer.len(),
        };
        let read = self.reader.read(&mut buffer[..limit])?;
        if let Some(left) = self.left.as_mut() {
            *left -= read;
            if !self.chunked && *left == 0 {
                self.done = true;
            }
        }
        if read == 0 {
            self.done = true;
        }
        Ok(read)
    }
}

pub struct Response {
    pub status: u16,
    pub body: BufReader<Body>,
}

/// One request to a loopback server; the body is read as it arrives.
pub fn request(
    method: &str,
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<Response> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let body = body.unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let lines = read_head(&mut reader)?;
    let status = lines
        .first()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or(std::io::ErrorKind::InvalidData)?;
    let headers = parse_headers(&lines[1..]);
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    let left = if chunked {
        None
    } else {
        headers
            .get("content-length")
            .and_then(|value| value.parse().ok())
    };
    Ok(Response {
        status,
        body: BufReader::new(Body {
            reader,
            chunked,
            done: left == Some(0),
            left,
        }),
    })
}

/// A random hex secret from the system's generator.
pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut buffer))
        .is_ok();
    assert!(filled, "the system's random generator is not readable");
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Constant-time comparison of a presented secret.
pub fn same_secret(presented: &str, expected: &str) -> bool {
    presented.len() == expected.len()
        && presented
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// A free loopback port for a child to listen on.
pub fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("no free loopback port: {error}"))
}
