//! `promptlyd status` — query a running daemon over its localhost API.
//!
//! The CLI (`19`) reads the machine-readable API directly; this command is the
//! daemon's own quick human check, reporting connected / capturing / idle. To
//! avoid pulling an HTTP client for a single loopback GET, it speaks the minimal
//! HTTP/1.1 needed to read `/session`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_millis(1_500);

/// The daemon's reported state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    /// Reachable and capturing the named session.
    Capturing { session_id: String, turns: u64 },
    /// Reachable but with no active capture session.
    Idle,
    /// Not reachable on the API address.
    NotRunning,
}

/// Query the daemon at `addr` and classify its state.
pub fn query(addr: SocketAddr) -> DaemonStatus {
    let body = match http_get(addr, "/session") {
        Ok((200, body)) => body,
        Ok(_) => return DaemonStatus::Idle,
        Err(_) => return DaemonStatus::NotRunning,
    };
    let Ok(json) = serde_json::from_str::<Value>(&body) else {
        return DaemonStatus::Idle;
    };
    match json
        .get("session")
        .and_then(|s| s.get("session_id"))
        .and_then(Value::as_str)
    {
        Some(session_id) => DaemonStatus::Capturing {
            session_id: session_id.to_string(),
            turns: json.get("turns").and_then(Value::as_u64).unwrap_or(0),
        },
        None => DaemonStatus::Idle,
    }
}

/// A one-line human rendering of the status.
pub fn render(status: &DaemonStatus) -> String {
    match status {
        DaemonStatus::Capturing { session_id, turns } => {
            format!("promptlyd: capturing — session {session_id}, {turns} turns")
        }
        DaemonStatus::Idle => "promptlyd: connected, idle".to_string(),
        DaemonStatus::NotRunning => "promptlyd: not running".to_string(),
    }
}

/// Minimal blocking HTTP/1.1 GET over loopback. Returns `(status_code, body)`.
fn http_get(addr: SocketAddr, path: &str) -> std::io::Result<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;

    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    Ok((parse_status_code(&raw), split_body(&raw)))
}

fn parse_status_code(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn split_body(response: &str) -> String {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_line_and_body() {
        let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"turns\":3}";
        assert_eq!(parse_status_code(response), 200);
        assert_eq!(split_body(response), "{\"turns\":3}");
    }

    #[test]
    fn unreachable_address_reports_not_running() {
        // Port 1 on loopback has nothing listening.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(query(addr), DaemonStatus::NotRunning);
    }

    #[test]
    fn render_covers_every_state() {
        assert!(render(&DaemonStatus::Capturing {
            session_id: "s1".into(),
            turns: 4,
        })
        .contains("capturing"));
        assert!(render(&DaemonStatus::Idle).contains("idle"));
        assert!(render(&DaemonStatus::NotRunning).contains("not running"));
    }
}
