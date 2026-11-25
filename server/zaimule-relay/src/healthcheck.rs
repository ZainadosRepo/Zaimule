//! Sonde `<binaire> healthcheck` pour le `healthcheck` Docker : l'image distroless
//! n'embarque ni shell ni curl, le binaire interroge donc lui-même `/healthz`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

pub fn run(port: u16) -> ExitCode {
    match probe(port) {
        Ok(true) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

fn probe(port: u16) -> std::io::Result<bool> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")?;
    let mut head = [0u8; 16];
    let n = stream.read(&mut head)?;
    Ok(is_ok_status(&head[..n]))
}

fn is_ok_status(head: &[u8]) -> bool {
    head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200")
}

#[cfg(test)]
mod tests {
    use super::is_ok_status;

    #[test]
    fn only_200_is_healthy() {
        assert!(is_ok_status(b"HTTP/1.1 200 OK"));
        assert!(is_ok_status(b"HTTP/1.0 200 OK"));
        assert!(!is_ok_status(b"HTTP/1.1 503 Service Unavailable"));
        assert!(!is_ok_status(b""));
    }
}
