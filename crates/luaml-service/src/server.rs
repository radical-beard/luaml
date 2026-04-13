use std::net::TcpListener;
use std::path::Path;
use std::thread;

use crate::connection::handle_connection;

/// Listen mode for the service.
pub enum ListenAddr {
    Tcp(String),
    #[cfg(unix)]
    Unix(String),
}

impl ListenAddr {
    /// Parse a listen address string.
    ///
    /// Formats:
    /// - `tcp:HOST:PORT` — TCP socket
    /// - `unix:PATH` — Unix domain socket (Unix only)
    /// - `HOST:PORT` — TCP socket (default)
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(rest) = s.strip_prefix("tcp:") {
            Ok(ListenAddr::Tcp(rest.to_string()))
        } else if let Some(rest) = s.strip_prefix("unix:") {
            #[cfg(unix)]
            {
                Ok(ListenAddr::Unix(rest.to_string()))
            }
            #[cfg(not(unix))]
            {
                let _ = rest;
                Err("unix sockets are not supported on this platform".into())
            }
        } else {
            // Default: treat as TCP address.
            Ok(ListenAddr::Tcp(s.to_string()))
        }
    }
}

/// Start the server and block, accepting connections.
pub fn serve(addr: &ListenAddr) -> std::io::Result<()> {
    match addr {
        ListenAddr::Tcp(bind) => serve_tcp(bind),
        #[cfg(unix)]
        ListenAddr::Unix(path) => serve_unix(path),
    }
}

fn serve_tcp(bind: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind)?;
    eprintln!("listening on tcp://{bind}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || handle_connection(stream));
            }
            Err(e) => {
                eprintln!("accept error: {e}");
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
fn serve_unix(path: &str) -> std::io::Result<()> {
    use std::os::unix::net::UnixListener;

    // Remove stale socket file if it exists.
    let socket_path = Path::new(path);
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(path)?;
    eprintln!("listening on unix://{path}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    let reader: Box<dyn std::io::Read + Send> =
                        Box::new(stream.try_clone().expect("failed to clone unix stream"));
                    let writer: Box<dyn std::io::Write + Send> = Box::new(stream);
                    crate::connection::handle_stream(reader, writer);
                });
            }
            Err(e) => {
                eprintln!("accept error: {e}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tcp_prefix() {
        match ListenAddr::parse("tcp:127.0.0.1:9100").unwrap() {
            ListenAddr::Tcp(addr) => assert_eq!(addr, "127.0.0.1:9100"),
            #[cfg(unix)]
            _ => panic!("expected Tcp"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_unix_prefix() {
        match ListenAddr::parse("unix:/tmp/luaml.sock").unwrap() {
            ListenAddr::Unix(path) => assert_eq!(path, "/tmp/luaml.sock"),
            _ => panic!("expected Unix"),
        }
    }

    #[test]
    fn parse_bare_host_port() {
        match ListenAddr::parse("127.0.0.1:9100").unwrap() {
            ListenAddr::Tcp(addr) => assert_eq!(addr, "127.0.0.1:9100"),
            #[cfg(unix)]
            _ => panic!("expected Tcp"),
        }
    }

    #[test]
    fn parse_tcp_empty_after_prefix() {
        match ListenAddr::parse("tcp:").unwrap() {
            ListenAddr::Tcp(addr) => assert_eq!(addr, ""),
            #[cfg(unix)]
            _ => panic!("expected Tcp"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_unix_empty_after_prefix() {
        match ListenAddr::parse("unix:").unwrap() {
            ListenAddr::Unix(path) => assert_eq!(path, ""),
            _ => panic!("expected Unix"),
        }
    }

    #[test]
    fn parse_bare_empty_string() {
        match ListenAddr::parse("").unwrap() {
            ListenAddr::Tcp(addr) => assert_eq!(addr, ""),
            #[cfg(unix)]
            _ => panic!("expected Tcp"),
        }
    }

    #[test]
    fn parse_localhost_different_port() {
        match ListenAddr::parse("tcp:0.0.0.0:8080").unwrap() {
            ListenAddr::Tcp(addr) => assert_eq!(addr, "0.0.0.0:8080"),
            #[cfg(unix)]
            _ => panic!("expected Tcp"),
        }
    }

    #[test]
    fn parse_ipv6_address() {
        match ListenAddr::parse("tcp:[::1]:9100").unwrap() {
            ListenAddr::Tcp(addr) => assert_eq!(addr, "[::1]:9100"),
            #[cfg(unix)]
            _ => panic!("expected Tcp"),
        }
    }
}
