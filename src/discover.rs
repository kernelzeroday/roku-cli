use anyhow::Result;
use std::collections::HashSet;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

pub struct Found {
    pub ip: String,
}

pub fn discover(timeout_secs: u64) -> Result<Vec<Found>> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(700)))?;

    let msg = "M-SEARCH * HTTP/1.1\r\n\
        HOST: 239.255.255.250:1900\r\n\
        MAN: \"ssdp:discover\"\r\n\
        ST: roku:ecp\r\n\
        MX: 2\r\n\r\n";
    // UDP is lossy; send the probe a few times (responses are deduped below)
    for _ in 0..3 {
        socket.send_to(msg.as_bytes(), "239.255.255.250:1900")?;
    }

    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, src)) => {
                let resp = String::from_utf8_lossy(&buf[..n]);
                if resp.to_lowercase().contains("roku") || resp.contains(":8060") {
                    let ip = src.ip().to_string();
                    if seen.insert(ip.clone()) {
                        found.push(Found { ip });
                    }
                }
            }
            Err(_) => continue,
        }
    }
    Ok(found)
}
