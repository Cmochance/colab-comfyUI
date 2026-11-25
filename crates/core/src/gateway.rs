use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::task::JoinHandle;

use crate::config::REMOTE_LLM_PORT;
use crate::local_proxy::lookup_port_owner;
use crate::runner::CommandRunner;
use crate::CoreError;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Running local loopback API gateway forwarding `127.0.0.1:<port>` to `llm.sock`.
pub struct GatewayHandle {
    pub port: u16,
    conns: Arc<StdMutex<Vec<JoinHandle<()>>>>,
    task: Option<JoinHandle<()>>,
}

impl GatewayHandle {
    pub async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        let handles = {
            let mut guard = self.conns.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for h in handles {
            h.abort();
            let _ = h.await;
        }
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Ok(mut guard) = self.conns.lock() {
            for h in guard.drain(..) {
                h.abort();
            }
        }
    }
}

/// Starts the authenticated transparent API gateway on fixed `127.0.0.1:<bind_port>`.
/// Never falls back to another port on `AddrInUse`.
pub async fn start_gateway_proxy(
    bind_port: u16,
    llm_sock_path: PathBuf,
    api_key: String,
    runner: &dyn CommandRunner,
) -> Result<GatewayHandle, CoreError> {
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
                context: format!("bind gateway 127.0.0.1:{bind_port}"),
                source,
            });
        }
    };

    let actual_port = listener
        .local_addr()
        .map_err(|source| CoreError::Io {
            context: "gateway local_addr".to_string(),
            source,
        })?
        .port();

    let conns: Arc<StdMutex<Vec<JoinHandle<()>>>> = Arc::new(StdMutex::new(Vec::new()));
    let conns_ref = conns.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _peer)) = listener.accept().await {
            let sock = llm_sock_path.clone();
            let key = api_key.clone();
            let h = tokio::spawn(async move {
                let _ = handle_gateway_connection(stream, actual_port, sock, key).await;
            });
            if let Ok(mut guard) = conns_ref.lock() {
                guard.retain(|existing| !existing.is_finished());
                guard.push(h);
            }
        }
    });

    Ok(GatewayHandle {
        port: actual_port,
        conns,
        task: Some(task),
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn is_allowed_host(host: &str, port: u16) -> bool {
    host.eq_ignore_ascii_case(&format!("127.0.0.1:{port}"))
        || host.eq_ignore_ascii_case(&format!("localhost:{port}"))
}

fn is_allowed_origin(origin: &str, port: u16) -> bool {
    origin.eq_ignore_ascii_case(&format!("http://127.0.0.1:{port}"))
        || origin.eq_ignore_ascii_case(&format!("http://localhost:{port}"))
}

async fn write_json_error(
    stream: &mut TcpStream,
    status_line: &str,
    err_type: &str,
    message: &str,
) -> std::io::Result<()> {
    let body = serde_json::json!({
        "error": {
            "type": err_type,
            "message": message,
        }
    })
    .to_string();
    let resp = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

fn is_allowed_endpoint(method: &str, raw_path: &str) -> bool {
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    matches!(
        (method, path),
        ("POST", "/v1/messages")
            | ("POST", "/v1/messages/count_tokens")
            | ("POST", "/v1/chat/completions")
            | ("GET", "/v1/models")
            | ("GET", "/health")
    )
}

async fn handle_gateway_connection(
    mut client: TcpStream,
    port: u16,
    llm_sock: PathBuf,
    expected_key: String,
) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    let header_end = tokio::time::timeout(HEADER_READ_TIMEOUT, async {
        loop {
            let n = client.read(&mut tmp).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF before HTTP headers",
                ));
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(pos + 4);
            }
            if buf.len() > MAX_HEADER_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HTTP header too large",
                ));
            }
        }
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "header timeout"))??;

    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut req = httparse::Request::new(&mut headers);
    let parsed = matches!(
        req.parse(&buf[..header_end]),
        Ok(httparse::Status::Complete(_))
    );
    if !parsed {
        return write_json_error(
            &mut client,
            "400 Bad Request",
            "invalid_request_error",
            "Malformed HTTP request",
        )
        .await;
    }

    let (Some(method), Some(path)) = (req.method, req.path) else {
        return write_json_error(
            &mut client,
            "400 Bad Request",
            "invalid_request_error",
            "Missing HTTP method or path",
        )
        .await;
    };

    let mut host_count = 0usize;
    let mut host_all_ok = true;
    let mut origin_all_ok = true;
    let mut provided_keys: Vec<String> = Vec::new();
    let mut forwarded_headers = Vec::new();

    for h in req.headers.iter() {
        let name = h.name;
        let val = String::from_utf8_lossy(h.value).trim().to_string();
        if name.eq_ignore_ascii_case("host") {
            host_count += 1;
            if !is_allowed_host(&val, port) {
                host_all_ok = false;
            }
            continue;
        }
        if name.eq_ignore_ascii_case("origin") {
            if !is_allowed_origin(&val, port) {
                origin_all_ok = false;
            }
            continue;
        }
        if name.eq_ignore_ascii_case("authorization") {
            let key = val
                .strip_prefix("Bearer ")
                .or_else(|| val.strip_prefix("bearer "))
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            provided_keys.push(key);
            continue;
        }
        if name.eq_ignore_ascii_case("x-api-key") {
            provided_keys.push(val);
            continue;
        }
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("keep-alive")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("upgrade")
            || name.eq_ignore_ascii_case("proxy-authenticate")
            || name.eq_ignore_ascii_case("proxy-authorization")
        {
            continue;
        }
        forwarded_headers.push(format!("{name}: {val}"));
    }

    if host_count != 1 || !host_all_ok || !origin_all_ok {
        return write_json_error(
            &mut client,
            "403 Forbidden",
            "forbidden_origin_or_host",
            "Rejected Host or Origin",
        )
        .await;
    }

    let authed = !provided_keys.is_empty()
        && provided_keys
            .iter()
            .all(|k| constant_time_eq(k.as_bytes(), expected_key.as_bytes()));
    if !authed {
        return write_json_error(
            &mut client,
            "401 Unauthorized",
            "authentication_error",
            "Invalid or missing API key",
        )
        .await;
    }

    if !is_allowed_endpoint(method, path) {
        return write_json_error(
            &mut client,
            "404 Not Found",
            "not_found_error",
            "Endpoint not exposed by gateway",
        )
        .await;
    }

    let mut upstream = match UnixStream::connect(&llm_sock).await {
        Ok(u) => u,
        Err(_) => {
            return write_json_error(
                &mut client,
                "502 Bad Gateway",
                "upstream_unavailable",
                "Remote llama-server sidecar is not running",
            )
            .await;
        }
    };

    let mut rewritten = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{REMOTE_LLM_PORT}\r\nConnection: close\r\n"
    );
    for h in forwarded_headers {
        rewritten.push_str(&h);
        rewritten.push_str("\r\n");
    }
    rewritten.push_str("\r\n");

    upstream.write_all(rewritten.as_bytes()).await?;
    if buf.len() > header_end {
        upstream.write_all(&buf[header_end..]).await?;
    }

    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, LineCallback};
    use std::future::Future;
    use std::pin::Pin;
    use tokio::net::UnixListener;

    struct NoopRunner;
    impl CommandRunner for NoopRunner {
        fn run<'a>(
            &'a self,
            _spec: crate::runner::CommandSpec,
            _on_line: Option<LineCallback>,
        ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CoreError>> + Send + 'a>> {
            Box::pin(async move {
                Ok(CommandOutput::from_raw(
                    Some(0),
                    String::new(),
                    String::new(),
                ))
            })
        }
    }

    async fn send_raw(port: u16, req: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).to_string()
    }

    #[test]
    fn gateway_enforces_auth_host_origin_and_proxies_native_endpoints() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!("ccu_gw_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let sock_path = dir.join("llm.sock");
            let _ = std::fs::remove_file(&sock_path);

            let listener = UnixListener::bind(&sock_path).unwrap();
            tokio::spawn(async move {
                while let Ok((mut u, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        let n = u.read(&mut buf).await.unwrap();
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let is_anthropic = req.starts_with("POST /v1/messages");
                        let body = if is_anthropic {
                            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n"
                        } else {
                            "{\"id\":\"chatcmpl-1\"}"
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = u.write_all(resp.as_bytes()).await;
                    });
                }
            });

            let key = "ccu_0123456789abcdef0123456789abcdef".to_string();
            let gw = start_gateway_proxy(0, sock_path.clone(), key.clone(), &NoopRunner)
                .await
                .unwrap();
            let port = gw.port;

            // 1. Missing auth -> 401
            let r1 = send_raw(
                port,
                &format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r1.starts_with("HTTP/1.1 401"));

            // 2. Bad Origin -> 403
            let r2 = send_raw(
                port,
                &format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nx-api-key: {key}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r2.starts_with("HTTP/1.1 403"));

            // 3. Valid Anthropic x-api-key -> 200 SSE passthrough
            let r3 = send_raw(
                port,
                &format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-api-key: {key}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r3.starts_with("HTTP/1.1 200"));
            assert!(r3.contains("event: message_start"));

            // 4. Valid OpenAI Bearer -> 200 passthrough + Connection: close injected
            let r4 = send_raw(
                port,
                &format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: keep-alive\r\nAuthorization: Bearer {key}\r\n\r\n"),
            )
            .await;
            assert!(r4.starts_with("HTTP/1.1 200"));
            assert!(r4.contains("chatcmpl-1"));

            // 5. Wrong Host / multi Origin / multi Host -> 403
            let r5 = send_raw(
                port,
                &format!("POST /v1/messages HTTP/1.1\r\nHost: evil.example:{port}\r\nx-api-key: {key}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r5.starts_with("HTTP/1.1 403"));

            let r6 = send_raw(
                port,
                &format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nOrigin: http://127.0.0.1:{port}\r\nx-api-key: {key}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r6.starts_with("HTTP/1.1 403"));

            // 6. Wrong key -> 401
            let r7 = send_raw(
                port,
                &format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-api-key: ccu_wrong\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r7.starts_with("HTTP/1.1 401"));

            // 7. Non-allowlisted path -> 404
            let r8 = send_raw(
                port,
                &format!("GET /props HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {key}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r8.starts_with("HTTP/1.1 404"));

            // 8. /v1/models & /v1/messages/count_tokens -> 200
            let r9 = send_raw(
                port,
                &format!("GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {key}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r9.starts_with("HTTP/1.1 200"));

            let r10 = send_raw(
                port,
                &format!("POST /v1/messages/count_tokens HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-api-key: {key}\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(r10.starts_with("HTTP/1.1 200"));

            gw.shutdown().await;
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
