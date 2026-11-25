use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::task::JoinHandle;

use crate::locate::LSOF_BIN;
use crate::runner::{CommandRunner, CommandSpec};
use crate::CoreError;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Running local loopback reverse proxy forwarding `127.0.0.1:<port>` to `comfy.sock`.
pub struct LocalProxyHandle {
    pub port: u16,
    task: Option<JoinHandle<()>>,
}

impl LocalProxyHandle {
    pub async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for LocalProxyHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Starts the `Host`/`Origin`-validated reverse proxy on fixed `127.0.0.1:<bind_port>`.
/// Never falls back to another port on `AddrInUse`.
pub async fn start_local_proxy(
    bind_port: u16,
    comfy_sock_path: PathBuf,
    runner: &dyn CommandRunner,
) -> Result<LocalProxyHandle, CoreError> {
    let listener = match TcpListener::bind(("127.0.0.1", bind_port)).await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            let (pid, process_name) = lookup_port_owner(runner, bind_port).await;
            return Err(CoreError::PortInUse {
                port: bind_port,
                pid,
                process_name,
            });
        }
        Err(source) => {
            return Err(CoreError::Io {
                context: format!("bind 127.0.0.1:{bind_port}"),
                source,
            });
        }
    };

    let actual_port = listener
        .local_addr()
        .map_err(|source| CoreError::Io {
            context: "proxy local_addr".to_string(),
            source,
        })?
        .port();

    let expected_host = format!("127.0.0.1:{actual_port}");
    let expected_origin = format!("http://127.0.0.1:{actual_port}");

    let task = tokio::spawn(async move {
        while let Ok((stream, _peer)) = listener.accept().await {
            let host = expected_host.clone();
            let origin = expected_origin.clone();
            let sock = comfy_sock_path.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, host, origin, sock).await;
            });
        }
    });

    Ok(LocalProxyHandle {
        port: actual_port,
        task: Some(task),
    })
}

pub(crate) async fn lookup_port_owner(
    runner: &dyn CommandRunner,
    port: u16,
) -> (Option<u32>, Option<String>) {
    let lsof = Path::new(LSOF_BIN);
    if !lsof.exists() {
        return (None, None);
    }
    let spec = CommandSpec::new(
        lsof,
        vec![
            "-nP".to_string(),
            format!("-iTCP:{port}"),
            "-sTCP:LISTEN".to_string(),
            "-Fpc".to_string(),
        ],
        Duration::from_secs(3),
    );
    let Ok(out) = runner.run(spec, None).await else {
        return (None, None);
    };
    let mut pid = None;
    let mut name = None;
    for line in out.stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            if pid.is_none() {
                pid = rest.trim().parse::<u32>().ok();
            }
        } else if let Some(rest) = line.strip_prefix('c') {
            if name.is_none() {
                name = Some(rest.trim().to_string());
            }
        }
    }
    (pid, name)
}

async fn write_error_response(
    stream: &mut TcpStream,
    status_line: &str,
    body: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

fn header_eq_ci(name: &str, target: &str) -> bool {
    name.eq_ignore_ascii_case(target)
}

fn contains_token_ci(value: &str, token: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .any(|part| part.eq_ignore_ascii_case(token))
}

async fn handle_connection(
    mut client: TcpStream,
    expected_host: String,
    expected_origin: String,
    comfy_sock: PathBuf,
) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    let (header_len, is_ws, rewritten_head) = tokio::time::timeout(HEADER_READ_TIMEOUT, async {
        loop {
            let n = client.read(&mut tmp).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof before request headers",
                ));
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.len() > MAX_HEADER_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request headers too large",
                ));
            }

            let mut headers = [httparse::EMPTY_HEADER; 128];
            let mut req = httparse::Request::new(&mut headers);
            match req.parse(&buf) {
                Ok(httparse::Status::Complete(hlen)) => {
                    let method = req.method.unwrap_or("GET").to_string();
                    let path = req.path.unwrap_or("/").to_string();
                    let version = req.version.unwrap_or(1);

                    let mut hosts = Vec::new();
                    let mut origins = Vec::new();
                    let mut conn_upgrade = false;
                    let mut upgrade_ws = false;
                    let mut raw_headers = Vec::new();

                    for h in req.headers.iter() {
                        let val = std::str::from_utf8(h.value)
                            .map_err(|_| {
                                std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "non-utf8 header",
                                )
                            })?
                            .trim();
                        if header_eq_ci(h.name, "host") {
                            hosts.push(val.to_string());
                        } else if header_eq_ci(h.name, "origin") {
                            origins.push(val.to_string());
                        } else if header_eq_ci(h.name, "connection") {
                            conn_upgrade = contains_token_ci(val, "upgrade");
                        } else if header_eq_ci(h.name, "upgrade") {
                            upgrade_ws = val.eq_ignore_ascii_case("websocket");
                        }
                        if !header_eq_ci(h.name, "connection") {
                            raw_headers.push((h.name.to_string(), val.to_string()));
                        }
                    }

                    let host_ok = hosts.len() == 1 && hosts[0] == expected_host;
                    let origin_ok = origins.iter().all(|o| o == &expected_origin);
                    if !host_ok || !origin_ok {
                        return Ok((hlen, false, None));
                    }

                    let is_ws = conn_upgrade && upgrade_ws;
                    if is_ws {
                        return Ok((hlen, true, Some(buf[..hlen].to_vec())));
                    }

                    let mut out = format!("{method} {path} HTTP/1.{version}\r\n");
                    for (k, v) in raw_headers {
                        out.push_str(&k);
                        out.push_str(": ");
                        out.push_str(&v);
                        out.push_str("\r\n");
                    }
                    out.push_str("Connection: close\r\n\r\n");
                    return Ok((hlen, false, Some(out.into_bytes())));
                }
                Ok(httparse::Status::Partial) => continue,
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "malformed http request",
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "header timeout"))??;

    let Some(head_bytes) = rewritten_head else {
        return write_error_response(
            &mut client,
            "403 Forbidden",
            "Forbidden: invalid Host or Origin",
        )
        .await;
    };

    let mut upstream = match UnixStream::connect(&comfy_sock).await {
        Ok(u) => u,
        Err(_) => {
            return write_error_response(
                &mut client,
                "502 Bad Gateway",
                "ComfyUI tunnel unavailable",
            )
            .await;
        }
    };

    upstream.write_all(&head_bytes).await?;
    if buf.len() > header_len {
        upstream.write_all(&buf[header_len..]).await?;
    }
    if !is_ws {
        upstream.flush().await?;
    }

    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::TokioCommandRunner;
    use std::fs;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::net::UnixListener;

    struct ShortTempDir(PathBuf);

    impl ShortTempDir {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = PathBuf::from(format!(
                "/tmp/ccu-px-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for ShortTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn proxy_enforces_host_origin_and_forwards_http_and_websocket() {
        let tmp = ShortTempDir::new();
        let sock_path = tmp.0.join("comfy.sock");

        block_on(async {
            let upstream_hits = Arc::new(AtomicUsize::new(0));
            let listener = UnixListener::bind(&sock_path).unwrap();
            let hits_clone = upstream_hits.clone();
            let upstream_task = tokio::spawn(async move {
                loop {
                    let Ok((mut conn, _)) = listener.accept().await else {
                        break;
                    };
                    hits_clone.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let n = conn.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]);
                        if req.contains("Upgrade: websocket") {
                            let _ = conn
                                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
                                .await;
                            let mut echo = [0u8; 64];
                            if let Ok(m) = conn.read(&mut echo).await {
                                if m > 0 {
                                    let _ = conn.write_all(&echo[..m]).await;
                                }
                            }
                        } else {
                            let _ = conn
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                                .await;
                        }
                    });
                }
            });

            let runner = TokioCommandRunner;
            let proxy = start_local_proxy(0, sock_path.clone(), &runner)
                .await
                .unwrap();
            let port = proxy.port;

            // 1. Valid Host passes and hits upstream
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(
                    format!("GET /system_stats HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut resp = String::new();
            stream.read_to_string(&mut resp).await.unwrap();
            assert!(resp.starts_with("HTTP/1.1 200 OK"));
            assert!(resp.ends_with("ok"));
            assert_eq!(upstream_hits.load(Ordering::SeqCst), 1);

            // 2. DNS rebinding / wrong Host / wrong Origin rejected with 403 and ZERO upstream hits
            for bad_req in [
                format!("GET / HTTP/1.1\r\nHost: evil.rebind:{port}\r\n\r\n"),
                format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n"),
                "GET / HTTP/1.1\r\n\r\n".to_string(),
                format!(
                    "POST /prompt HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.com\r\n\r\n"
                ),
            ] {
                let mut bad = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                bad.write_all(bad_req.as_bytes()).await.unwrap();
                let mut bad_resp = String::new();
                bad.read_to_string(&mut bad_resp).await.unwrap();
                assert!(
                    bad_resp.starts_with("HTTP/1.1 403 Forbidden"),
                    "expected 403 for {bad_req:?}, got {bad_resp:?}"
                );
            }
            assert_eq!(upstream_hits.load(Ordering::SeqCst), 1);

            // 3. WebSocket upgrade tunnels bidirectionally
            let mut ws = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            ws.write_all(
                format!(
                    "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            let mut head = [0u8; 256];
            let n = ws.read(&mut head).await.unwrap();
            assert!(String::from_utf8_lossy(&head[..n]).starts_with("HTTP/1.1 101"));
            ws.write_all(b"ping").await.unwrap();
            let mut pong = [0u8; 4];
            ws.read_exact(&mut pong).await.unwrap();
            assert_eq!(&pong, b"ping");

            // 4. Fixed port conflict returns PortInUse without incrementing
            let conflict = start_local_proxy(port, sock_path, &runner).await;
            assert!(matches!(conflict, Err(CoreError::PortInUse { port: p, .. }) if p == port));

            proxy.shutdown().await;
            upstream_task.abort();
        });
    }
}
