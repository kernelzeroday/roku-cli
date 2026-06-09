use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub struct Roku {
    host: String,
}

pub struct App {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
}

impl Roku {
    pub fn new(ip: &str) -> Self {
        Roku {
            host: format!("{}:8060", ip),
        }
    }

    fn post(&self, path: &str) -> Result<()> {
        let (code, _) = http_request(&self.host, "POST", &format!("/{}", path))?;
        if !(200..300).contains(&code) {
            bail!("Roku returned HTTP {} for /{}", code, path);
        }
        Ok(())
    }

    fn get(&self, path: &str) -> Result<String> {
        let (code, body) = http_request(&self.host, "GET", &format!("/{}", path))?;
        if !(200..300).contains(&code) {
            bail!("Roku returned HTTP {} for /{}", code, path);
        }
        Ok(body)
    }

    pub fn keypress(&self, key: &str) -> Result<()> {
        self.post(&format!("keypress/{}", key))
    }

    pub fn type_text(&self, text: &str) -> Result<()> {
        for ch in text.chars() {
            self.post(&format!("keypress/Lit_{}", percent_encode(&ch.to_string())))?;
        }
        Ok(())
    }

    pub fn launch(&self, app_id: &str, params: &[(String, String)]) -> Result<()> {
        let mut path = format!("launch/{}", app_id);
        if !params.is_empty() {
            let query: Vec<String> = params
                .iter()
                .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
                .collect();
            path.push('?');
            path.push_str(&query.join("&"));
        }
        self.post(&path)
    }

    pub fn device_info(&self) -> Result<String> {
        self.get("query/device-info")
    }

    pub fn active_app_name(&self) -> Result<String> {
        let xml = self.get("query/active-app")?;
        let name = tag_text(&xml, "app").unwrap_or_else(|| "unknown".to_string());
        let id = xml.find("<app").and_then(|i| attr(&xml[i..], "id"));
        Ok(match id {
            Some(i) => format!("{} (id {})", name, i),
            None => name,
        })
    }

    pub fn apps(&self) -> Result<Vec<App>> {
        Ok(parse_apps(&self.get("query/apps")?))
    }

    pub fn media_player(&self) -> Result<String> {
        self.get("query/media-player")
    }
}

pub fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut search = xml;
    loop {
        let i = search.find(&open)?;
        let after = &search[i + open.len()..];
        // Require a delimiter so "<app" doesn't match "<apps>"
        if !matches!(after.chars().next(), Some('>' | '/' | ' ' | '\t' | '\r' | '\n')) {
            search = after;
            continue;
        }
        let gt = after.find('>')? + 1;
        let rest = &after[gt..];
        let end = rest.find(&close)?;
        let text = rest[..end].trim();
        return if text.is_empty() {
            None
        } else {
            Some(xml_unescape(text))
        };
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn attr(s: &str, key: &str) -> Option<String> {
    let pat = format!("{}=\"", key);
    let start = s.find(&pat)? + pat.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn parse_apps(xml: &str) -> Vec<App> {
    let mut out = Vec::new();
    for part in xml.split("<app ").skip(1) {
        let id = match attr(part, "id") {
            Some(i) => i,
            None => continue,
        };
        let version = attr(part, "version");
        let name = part
            .split_once('>')
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split('<').next())
            .unwrap_or("")
            .trim()
            .to_string();
        let name = xml_unescape(&name);
        out.push(App { id, name, version });
    }
    out
}

fn http_request(host: &str, method: &str, path: &str) -> Result<(u16, String)> {
    let addr = host
        .to_socket_addrs()
        .with_context(|| format!("invalid Roku address '{}'", host))?
        .next()
        .with_context(|| format!("could not resolve '{}'", host))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .with_context(|| format!("could not connect to {}", host))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: */*\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes())?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut header_end = None;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if header_end.is_none() {
            if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
                header_end = Some(p);
            }
        }
        if let Some(p) = header_end {
            let head = String::from_utf8_lossy(&buf[..p]);
            let body_start = p + 4;
            match content_length(&head) {
                Some(len) if buf.len() - body_start >= len => break,
                None => continue, // no length: read until server closes (EOF above)
                _ => {}
            }
        }
    }

    let p = header_end.context("malformed HTTP response (no header terminator)")?;
    let head = String::from_utf8_lossy(&buf[..p]);
    let status_line = head.lines().next().context("empty HTTP response")?;
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .context("could not parse HTTP status code")?;
    let body = String::from_utf8_lossy(&buf[p + 4..]).to_string();
    Ok((code, body))
}

fn content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse().ok();
            }
        }
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_text_requires_exact_tag() {
        let xml = "<apps><app id=\"1\">One</app></apps>";
        assert_eq!(tag_text(xml, "app").as_deref(), Some("One"));
        let xml = "<power-mode-reason>x</power-mode-reason><power-mode>PowerOn</power-mode>";
        assert_eq!(tag_text(xml, "power-mode").as_deref(), Some("PowerOn"));
    }

    #[test]
    fn tag_text_unescapes_entities() {
        let xml = "<name>Ben &amp; Jerry&apos;s</name>";
        assert_eq!(tag_text(xml, "name").as_deref(), Some("Ben & Jerry's"));
    }

    #[test]
    fn tag_text_empty_and_missing() {
        assert_eq!(tag_text("<a></a>", "a"), None);
        assert_eq!(tag_text("<a>x</a>", "b"), None);
    }

    #[test]
    fn parse_apps_basic() {
        let xml = r#"<apps>
            <app id="12" version="1.0">Netflix</app>
            <app id="837">YouTube &amp; TV</app>
        </apps>"#;
        let apps = parse_apps(xml);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].id, "12");
        assert_eq!(apps[0].name, "Netflix");
        assert_eq!(apps[0].version.as_deref(), Some("1.0"));
        assert_eq!(apps[1].name, "YouTube & TV");
        assert_eq!(apps[1].version, None);
    }

    #[test]
    fn percent_encode_reserved() {
        assert_eq!(percent_encode("a b&c=д"), "a%20b%26c%3D%D0%B4");
    }

    #[test]
    fn content_length_case_insensitive() {
        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\ncontent-LENGTH: 42\r\n"),
            Some(42)
        );
        assert_eq!(content_length("HTTP/1.1 200 OK\r\n"), None);
    }
}
