//! Minimal HTTP/1 head handling for the command proxy.

use std::io;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;

const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Hop-by-hop headers that belong to the client's connection with this proxy.
const PROXY_HEADERS: &[&str] = &["proxy-authorization", "proxy-connection", "connection"];

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RequestHead {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) version: String,
    pub(crate) headers: Vec<(String, String)>,
}

impl RequestHead {
    pub(crate) fn parse(head: &[u8]) -> io::Result<Self> {
        let text = std::str::from_utf8(head).map_err(|_| invalid("request head is not UTF-8"))?;
        let mut lines = text.split("\r\n");
        let mut request_line = lines.next().unwrap_or_default().split(' ');
        let (Some(method), Some(target), Some(version), None) = (
            request_line.next(),
            request_line.next(),
            request_line.next(),
            request_line.next(),
        ) else {
            return Err(invalid("malformed request line"));
        };
        let mut headers = Vec::new();
        for line in lines.take_while(|line| !line.is_empty()) {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| invalid("malformed header"))?;
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
        Ok(Self {
            method: method.to_string(),
            target: target.to_string(),
            version: version.to_string(),
            headers,
        })
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Serializes the head for the next hop: the client's proxy headers are replaced by `extra`.
    pub(crate) fn to_bytes(&self, target: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let mut head = format!("{} {target} {}\r\n", self.method, self.version);
        let forwarded = self.headers.iter().filter(|(name, _)| {
            !PROXY_HEADERS
                .iter()
                .any(|proxy_header| name.eq_ignore_ascii_case(proxy_header))
        });
        for (name, value) in forwarded {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        for (name, value) in extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        head.into_bytes()
    }
}

/// Reads a head; returns it and any bytes already read past its end.
pub(crate) async fn read_head<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Ok((buffer, rest));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(invalid("head too large"));
        }
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// The status code of a response head such as `HTTP/1.1 200 Connection established`.
pub(crate) fn status_code(head: &[u8]) -> Option<u16> {
    let line = head.split(|byte| *byte == b'\r').next()?;
    let mut parts = std::str::from_utf8(line).ok()?.split(' ');
    parts.next()?.starts_with("HTTP/").then_some(())?;
    parts.next()?.parse().ok()
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
