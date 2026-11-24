use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::CoreError;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type LineCallback = Arc<dyn Fn(String) + Send + Sync>;

/// Controls child process stdin handling.
///
/// All `colab` commands except interactive `drivemount` (Phase 2) must use `Null`
/// so missing credentials hit EOF immediately instead of hanging in `input()`.
#[derive(Debug, Clone)]
pub enum StdinPolicy {
    Null,
    Bytes(Vec<u8>),
}

/// Process specification passed to `CommandRunner`.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub stdin: StdinPolicy,
    pub timeout: Duration,
    pub new_session: bool,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            args,
            envs: Vec::new(),
            stdin: StdinPolicy::Null,
            timeout,
            new_session: false,
        }
    }
}

/// Process output returned by `CommandRunner`.
///
/// `stdout` and `stderr` are always redacted for logs/UI. Raw unredacted text
/// stays crate-private so `colab_cli.rs` can hash endpoints without leaking them.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub(crate) stdout_raw: String,
    pub(crate) stderr_raw: String,
}

impl CommandOutput {
    pub fn from_raw(exit_code: Option<i32>, stdout_raw: String, stderr_raw: String) -> Self {
        Self {
            exit_code,
            stdout: redact_sensitive_text(&stdout_raw),
            stderr: redact_sensitive_text(&stderr_raw),
            stdout_raw,
            stderr_raw,
        }
    }
}

/// Trait abstracting short-lived subprocess execution for offline mocking.
pub trait CommandRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        spec: CommandSpec,
        on_line: Option<LineCallback>,
    ) -> BoxFuture<'a, Result<CommandOutput, CoreError>>;
}

/// Production `tokio::process` command runner with line streaming, timeout, and redaction.
#[derive(Debug, Default)]
pub struct TokioCommandRunner;

impl CommandRunner for TokioCommandRunner {
    fn run<'a>(
        &'a self,
        spec: CommandSpec,
        on_line: Option<LineCallback>,
    ) -> BoxFuture<'a, Result<CommandOutput, CoreError>> {
        Box::pin(async move {
            let cmd_label = spec.program.display().to_string();
            let mut cmd = Command::new(&spec.program);
            cmd.kill_on_drop(true);
            cmd.args(&spec.args);
            for (k, v) in &spec.envs {
                cmd.env(k, v);
            }
            match &spec.stdin {
                StdinPolicy::Null => {
                    cmd.stdin(Stdio::null());
                }
                StdinPolicy::Bytes(_) => {
                    cmd.stdin(Stdio::piped());
                }
            }
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            if spec.new_session {
                unsafe {
                    cmd.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }

            let mut child = cmd.spawn().map_err(|source| CoreError::Io {
                context: format!("spawn {cmd_label}"),
                source,
            })?;

            if let StdinPolicy::Bytes(bytes) = spec.stdin {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&bytes).await;
                    let _ = stdin.shutdown().await;
                }
            }

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            let (tx, mut rx) = mpsc::unbounded_channel::<(bool, String)>();

            let out_task = tokio::spawn({
                let tx = tx.clone();
                async move {
                    if let Some(out) = stdout {
                        let mut lines = BufReader::new(out).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let _ = tx.send((false, line));
                        }
                    }
                }
            });

            let err_task = tokio::spawn({
                let tx = tx.clone();
                async move {
                    if let Some(err) = stderr {
                        let mut lines = BufReader::new(err).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let _ = tx.send((true, line));
                        }
                    }
                }
            });
            drop(tx);

            let collect_fut = async {
                let mut raw_out = String::new();
                let mut raw_err = String::new();
                while let Some((is_err, line)) = rx.recv().await {
                    if let Some(cb) = &on_line {
                        cb(redact_sensitive_text(&line));
                    }
                    let target = if is_err { &mut raw_err } else { &mut raw_out };
                    if !target.is_empty() {
                        target.push('\n');
                    }
                    target.push_str(&line);
                }
                let _ = out_task.await;
                let _ = err_task.await;
                let status = child.wait().await.map_err(|source| CoreError::Io {
                    context: format!("wait {cmd_label}"),
                    source,
                })?;
                Ok::<CommandOutput, CoreError>(CommandOutput::from_raw(
                    status.code(),
                    raw_out,
                    raw_err,
                ))
            };

            match tokio::time::timeout(spec.timeout, collect_fut).await {
                Ok(result) => result,
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    Err(CoreError::CommandTimeout {
                        command: cmd_label,
                        timeout_secs: spec.timeout.as_secs(),
                    })
                }
            }
        })
    }
}

/// Scrubs proxy tokens, OAuth bearer tokens, header secrets, and Colab VM endpoints.
pub fn redact_sensitive_text(input: &str) -> String {
    let mut s = input.to_string();
    s = redact_key_value_ci(
        &s,
        "colab-runtime-proxy-token=",
        is_query_delim,
        "[REDACTED]",
    );
    s = redact_key_value_ci(&s, "access_token=", is_query_delim, "[REDACTED]");
    s = redact_header_ci(&s, "x-goog-colab-token", "[REDACTED]");
    s = redact_bearer_ci(&s);
    s = redact_tun_paths(&s);
    s = redact_session_line_endpoints(&s);
    s = redact_colab_urls(&s);
    s = redact_m_s_endpoints(&s);
    s
}

fn is_query_delim(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'&' | b'#' | b'"' | b'\'' | b'>' | b')')
}

fn is_url_delim(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'>' | b')' | b',' | b';')
}

fn is_tun_segment_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

fn find_ascii_ci(haystack: &str, needle: &str, start: usize) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || start + n.len() > h.len() {
        return None;
    }
    (start..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

fn redact_key_value_ci(
    input: &str,
    prefix: &str,
    stop: fn(u8) -> bool,
    replacement: &str,
) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while let Some(pos) = find_ascii_ci(input, prefix, idx) {
        let val_start = pos + prefix.len();
        out.push_str(&input[idx..val_start]);
        let bytes = input.as_bytes();
        let mut end = val_start;
        while end < bytes.len() && !stop(bytes[end]) {
            end += 1;
        }
        out.push_str(replacement);
        idx = end;
    }
    out.push_str(&input[idx..]);
    out
}

fn redact_header_ci(input: &str, header: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while let Some(pos) = find_ascii_ci(input, header, idx) {
        out.push_str(&input[idx..pos + header.len()]);
        let rest = &input[pos + header.len()..];
        let mut cursor = 0;
        let bytes = rest.as_bytes();
        if cursor < bytes.len() && matches!(bytes[cursor], b':' | b'=') {
            let sep = bytes[cursor] as char;
            out.push(sep);
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor] == b' ' {
                out.push(' ');
                cursor += 1;
            }
            while cursor < bytes.len() && !is_url_delim(bytes[cursor]) && bytes[cursor] != b'&' {
                cursor += 1;
            }
            out.push_str(replacement);
            idx = pos + header.len() + cursor;
        } else {
            idx = pos + header.len();
        }
    }
    out.push_str(&input[idx..]);
    out
}

fn redact_bearer_ci(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while let Some(pos) = find_ascii_ci(input, "bearer ", idx) {
        let val_start = pos + "bearer ".len();
        out.push_str(&input[idx..pos]);
        out.push_str("Bearer ");
        let bytes = input.as_bytes();
        let mut end = val_start;
        while end < bytes.len() && !is_url_delim(bytes[end]) {
            end += 1;
        }
        out.push_str("[REDACTED]");
        idx = end;
    }
    out.push_str(&input[idx..]);
    out
}

fn redact_tun_paths(input: &str) -> String {
    let s1 = redact_tun_prefix(input, "/tun/m/");
    redact_tun_prefix(&s1, "%2Ftun%2Fm%2F")
}

fn redact_tun_prefix(input: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while let Some(pos) = find_ascii_ci(input, prefix, idx) {
        let seg_start = pos + prefix.len();
        out.push_str(&input[idx..seg_start]);
        let bytes = input.as_bytes();
        let mut end = seg_start;
        while end < bytes.len() && is_tun_segment_char(bytes[end]) {
            end += 1;
        }
        if end > seg_start {
            out.push_str("[REDACTED]");
        }
        idx = end;
    }
    out.push_str(&input[idx..]);
    out
}

fn redact_session_line_endpoints(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix('[') {
                if let Some(bracket_end) = rest.find("] ") {
                    let after_bracket = &rest[bracket_end + 2..];
                    if let Some(hw_pos) = after_bracket.find(" | Hardware:") {
                        let name = &rest[..bracket_end];
                        let tail = &after_bracket[hw_pos..];
                        return format!("[{name}] [REDACTED_ENDPOINT]{tail}");
                    }
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_colab_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    let marker = ".colab.googleusercontent.com";
    while let Some(pos) = find_ascii_ci(input, marker, idx) {
        let bytes = input.as_bytes();
        let mut start = pos;
        while start > idx && !is_url_delim(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = pos + marker.len();
        while end < bytes.len() && !is_url_delim(bytes[end]) {
            end += 1;
        }
        out.push_str(&input[idx..start]);
        if input[start..pos].starts_with("wss://") {
            out.push_str("wss://[REDACTED_ENDPOINT]");
        } else if input[start..pos].starts_with("https://") {
            out.push_str("https://[REDACTED_ENDPOINT]");
        } else {
            out.push_str("[REDACTED_ENDPOINT]");
        }
        idx = end;
    }
    out.push_str(&input[idx..]);
    out
}

fn redact_m_s_endpoints(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while let Some(pos) = input[idx..].find("m-s-").map(|rel| idx + rel) {
        let prev_ok = pos == 0 || !input.as_bytes()[pos - 1].is_ascii_alphanumeric();
        if !prev_ok {
            out.push_str(&input[idx..pos + 4]);
            idx = pos + 4;
            continue;
        }
        let bytes = input.as_bytes();
        let mut end = pos + 4;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'-' | b'_'))
        {
            end += 1;
        }
        out.push_str(&input[idx..pos]);
        if end > pos + 4 {
            out.push_str("[REDACTED_ENDPOINT]");
            idx = end;
        } else {
            out.push_str("m-s-");
            idx = pos + 4;
        }
    }
    out.push_str(&input[idx..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn redact_covers_real_colab_cli_formats() {
        let sample = concat!(
            "[comfy-1234abcd] m-s-3x9abc123def | Hardware: NVIDIA L4 | Shape: Standard | Variant: GPU | Status: IDLE\n",
            "https://colab.research.google.com/notebooks/empty.ipynb?dbu=%2Ftun%2Fm%2Fm-s-3x9abc123def#datalabBackendUrl=https://colab.research.google.com/tun/m/m-s-3x9abc123def\n",
            "wss://8080-m-s-3x9abc123def.us-central1.colab.googleusercontent.com/colab/ssh?colab-runtime-proxy-token=tok_secret_123&colab-client-agent=colab-cli\n",
            "Authorization: Bearer ya29.a0AfH6SM_secret\n",
            "x-goog-colab-token: rt_secret_456\n",
            "https://oauth2.googleapis.com/tokeninfo?access_token=ya29.secret_qs"
        );

        let redacted = redact_sensitive_text(sample);
        for secret in [
            "m-s-3x9abc123def",
            "tok_secret_123",
            "ya29.a0AfH6SM_secret",
            "rt_secret_456",
            "ya29.secret_qs",
            "us-central1.colab.googleusercontent.com",
        ] {
            assert!(
                !redacted.contains(secret),
                "secret '{secret}' leaked in:\n{redacted}"
            );
        }
        assert!(redacted.contains("[comfy-1234abcd] [REDACTED_ENDPOINT] | Hardware: NVIDIA L4"));
        assert!(redacted.contains("%2Ftun%2Fm%2F[REDACTED]"));
        assert!(redacted.contains("/tun/m/[REDACTED]"));
        assert!(redacted.contains("Bearer [REDACTED]"));
        assert!(redacted.contains("x-goog-colab-token: [REDACTED]"));
    }

    #[test]
    fn runner_binds_stdin_null_and_enforces_timeout() {
        let runner = TokioCommandRunner;
        block_on(async {
            let eof_spec = CommandSpec::new(
                "/bin/sh",
                vec![
                    "-c".to_string(),
                    "if read line; then echo got:$line; else echo eof; fi".to_string(),
                ],
                Duration::from_secs(2),
            );
            let out = runner.run(eof_spec, None).await.unwrap();
            assert_eq!(out.exit_code, Some(0));
            assert_eq!(out.stdout.trim(), "eof");

            let timeout_spec = CommandSpec::new(
                "/bin/sleep",
                vec!["2".to_string()],
                Duration::from_millis(80),
            );
            let err = runner.run(timeout_spec, None).await.unwrap_err();
            assert!(matches!(err, CoreError::CommandTimeout { .. }));
        });
    }
}
