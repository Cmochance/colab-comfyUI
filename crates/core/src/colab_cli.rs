use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{sha256_hex, validate_session_name, GpuTier};
use crate::runner::{CommandOutput, CommandRunner, CommandSpec, LineCallback, StdinPolicy};
use crate::CoreError;

const CLI_SHORT_TIMEOUT: Duration = Duration::from_secs(45);
const CLI_NEW_TIMEOUT: Duration = Duration::from_secs(600);
const CLI_STOP_TIMEOUT: Duration = Duration::from_secs(60);
const COLAB_SHORT_REQ_TIMEOUT_SECS: &str = "30";
const COLAB_NEW_REQ_TIMEOUT_SECS: &str = "120";

/// Parsed output of `colab usage`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageInfo {
    pub current_balance: f64,
    pub usage_rate_hourly: f64,
    pub active_assignments: u32,
}

/// Parsed session row from `colab status` or `colab sessions`.
/// Stores `endpoint_sha256` instead of the raw endpoint URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    pub endpoint_sha256: String,
    pub hardware: String,
    pub shape: String,
    pub variant: String,
    pub status: Option<String>,
    pub is_orphan: bool,
}

/// Result of `colab status -s <session>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatusLookup {
    Present(SessionEntry),
    Missing,
}

/// Result of `colab stop -s <session>` + triple verification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StopReport {
    pub session_name: String,
    pub terminated_sentinel: bool,
    pub endpoint_absent: bool,
    pub owner_verified: bool,
    pub usage: Option<UsageInfo>,
    pub billing_verified_stopped: bool,
    pub summary: String,
    pub warnings: Vec<String>,
}

fn colab_spec(
    colab_bin: &Path,
    args: Vec<String>,
    timeout: Duration,
    req_timeout_secs: &str,
    extra_envs: &[(String, String)],
) -> CommandSpec {
    let mut spec = CommandSpec::new(colab_bin, args, timeout);
    spec.stdin = StdinPolicy::Null;
    spec.envs.extend(extra_envs.iter().cloned());
    spec.envs.push((
        "COLAB_CLI_REQUEST_TIMEOUT".to_string(),
        req_timeout_secs.to_string(),
    ));
    spec.envs
        .push(("PYTHONUNBUFFERED".to_string(), "1".to_string()));
    spec
}

/// Returns true for lines emitted by `colab`'s daily `auto_update` banner.
pub fn is_update_banner_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.starts_with("[colab] A new version of Colab CLI is available:")
        || trimmed.starts_with("[colab] You can run 'colab update --install'")
        || trimmed.starts_with("[colab] Run '")
        || trimmed.starts_with("[colab] To silence this check, set")
}

fn detect_auth_required(out: &CommandOutput) -> Option<String> {
    let combined = format!("{}\n{}", out.stdout_raw, out.stderr_raw);
    for marker in [
        "To authorize colab-cli",
        "Enter the authorization code:",
        "No valid default credentials",
        "failed to refresh credentials",
        "tokeninfo returned HTTP 400",
    ] {
        if combined.contains(marker) {
            return Some(out.stderr.clone());
        }
    }
    None
}

fn ensure_no_auth_error(out: &CommandOutput) -> Result<(), CoreError> {
    if let Some(detail) = detect_auth_required(out) {
        return Err(CoreError::AuthRequired { detail });
    }
    Ok(())
}

pub fn parse_whoami_output(out: &CommandOutput) -> Result<String, CoreError> {
    ensure_no_auth_error(out)?;
    if out.exit_code != Some(0) {
        return Err(CoreError::AuthRequired {
            detail: out.stderr.clone(),
        });
    }
    for line in out.stdout_raw.lines() {
        if is_update_banner_line(line) {
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix("Email:") {
            let email = rest.trim();
            if email.contains('@') && !email.contains('<') {
                return Ok(email.to_string());
            }
            return Err(CoreError::AuthRequired {
                detail: format!("unavailable email in colab whoami: {email}"),
            });
        }
    }
    Err(CoreError::AuthRequired {
        detail: "missing Email line in colab whoami output".to_string(),
    })
}

pub fn parse_usage_output(out: &CommandOutput) -> Result<UsageInfo, CoreError> {
    ensure_no_auth_error(out)?;
    if out.exit_code != Some(0) {
        return Err(CoreError::UsageUnverified {
            detail: format!("exit={:?} {}", out.exit_code, out.stderr),
        });
    }

    let mut balance = None;
    let mut rate = None;
    let mut assignments = None;

    for line in out.stdout_raw.lines() {
        if is_update_banner_line(line) {
            continue;
        }
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Current balance:") {
            let num = rest.split_whitespace().next().unwrap_or("");
            balance = num.parse::<f64>().ok();
        } else if let Some(rest) = t.strip_prefix("Usage rate:") {
            let num = rest.trim().strip_suffix("/hr").unwrap_or(rest.trim());
            rate = num.trim().parse::<f64>().ok();
        } else if let Some(rest) = t.strip_prefix("Active assignments:") {
            assignments = rest.trim().parse::<u32>().ok();
        }
    }

    match (balance, rate, assignments) {
        (Some(current_balance), Some(usage_rate_hourly), Some(active_assignments)) => {
            Ok(UsageInfo {
                current_balance,
                usage_rate_hourly,
                active_assignments,
            })
        }
        _ => Err(CoreError::UsageUnverified {
            detail: format!("incomplete usage output: {}", out.stdout),
        }),
    }
}

/// Parses `[name] endpoint | Hardware: X | Shape: Y | Variant: Z[ | Status: W]`.
pub fn parse_session_line(line: &str) -> Option<SessionEntry> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('[')?;
    let bracket_end = rest.find("] ")?;
    let name = rest[..bracket_end].trim().to_string();
    let after_name = &rest[bracket_end + 2..];
    let mut segments = after_name.split(" | ");
    let endpoint = segments.next()?.trim();
    if endpoint.is_empty() {
        return None;
    }

    let mut hardware = None;
    let mut shape = None;
    let mut variant = None;
    let mut status = None;

    for seg in segments {
        if let Some(v) = seg.strip_prefix("Hardware:") {
            hardware = Some(v.trim().to_string());
        } else if let Some(v) = seg.strip_prefix("Shape:") {
            shape = Some(v.trim().to_string());
        } else if let Some(v) = seg.strip_prefix("Variant:") {
            variant = Some(v.trim().to_string());
        } else if let Some(v) = seg.strip_prefix("Status:") {
            status = Some(v.trim().to_string());
        }
    }

    let hardware = hardware?;
    Some(SessionEntry {
        is_orphan: name == "?",
        name,
        endpoint_sha256: sha256_hex(endpoint.as_bytes()),
        hardware,
        shape: shape.unwrap_or_default(),
        variant: variant.unwrap_or_default(),
        status,
    })
}

pub fn parse_status_output(
    session_name: &str,
    out: &CommandOutput,
) -> Result<SessionStatusLookup, CoreError> {
    ensure_no_auth_error(out)?;
    if out.exit_code != Some(0) {
        return Err(CoreError::SessionsUnverified {
            detail: format!("colab status exit={:?}: {}", out.exit_code, out.stderr),
        });
    }

    let missing_marker = format!("[colab] Session '{session_name}' not found.");
    for line in out.stdout_raw.lines() {
        if is_update_banner_line(line) {
            continue;
        }
        let trimmed = line.trim();
        if trimmed == missing_marker {
            return Ok(SessionStatusLookup::Missing);
        }
        if let Some(entry) = parse_session_line(trimmed) {
            if entry.name == session_name && !entry.hardware.is_empty() {
                return Ok(SessionStatusLookup::Present(entry));
            }
        }
    }

    Err(CoreError::SessionsUnverified {
        detail: format!("unexpected colab status output: {}", out.stdout),
    })
}

pub fn parse_sessions_output(out: &CommandOutput) -> Result<Vec<SessionEntry>, CoreError> {
    ensure_no_auth_error(out)?;
    if out.exit_code != Some(0) {
        return Err(CoreError::SessionsUnverified {
            detail: format!("colab sessions exit={:?}: {}", out.exit_code, out.stderr),
        });
    }

    let mut entries = Vec::new();
    let mut saw_no_sessions = false;
    for line in out.stdout_raw.lines() {
        if is_update_banner_line(line) {
            continue;
        }
        let trimmed = line.trim();
        if trimmed == "[colab] No active sessions found on server." {
            saw_no_sessions = true;
            continue;
        }
        if let Some(entry) = parse_session_line(trimmed) {
            entries.push(entry);
        }
    }

    if !entries.is_empty() || saw_no_sessions {
        Ok(entries)
    } else {
        Err(CoreError::SessionsUnverified {
            detail: format!("unexpected colab sessions output: {}", out.stdout),
        })
    }
}

pub fn parse_new_output(gpu: GpuTier, out: &CommandOutput) -> Result<(), CoreError> {
    ensure_no_auth_error(out)?;
    if out
        .stdout_raw
        .lines()
        .any(|line| line.trim() == "[colab] Session READY.")
    {
        return Ok(());
    }

    let combined_raw = format!("{}\n{}", out.stdout_raw, out.stderr_raw);
    if combined_raw.contains("Backend rejected accelerator")
        || combined_raw.contains("not entitled")
        || combined_raw.contains("HTTP 400")
    {
        return Err(CoreError::NoAcceleratorQuota {
            gpu: gpu.as_cli_arg().to_string(),
            detail: out.stderr.clone(),
        });
    }
    if combined_raw.contains("Allocation refused (precondition failed)")
        || combined_raw.contains("412")
        || combined_raw.contains("too many active sessions")
    {
        return Err(CoreError::CapacityOrSessionLimit {
            detail: out.stderr.clone(),
        });
    }

    Err(CoreError::CommandFailed {
        command: "colab new".to_string(),
        exit_code: out.exit_code,
        stderr: if out.stderr.is_empty() {
            out.stdout.clone()
        } else {
            out.stderr.clone()
        },
    })
}

pub fn build_new_args(
    session_name: &str,
    gpu: GpuTier,
    high_mem: bool,
) -> Result<Vec<String>, CoreError> {
    validate_session_name(session_name)?;
    let mut args = vec![
        "new".to_string(),
        "-s".to_string(),
        session_name.to_string(),
        "--gpu".to_string(),
        gpu.as_cli_arg().to_string(),
    ];
    if high_mem && gpu.supports_high_mem() {
        args.push("--high-mem".to_string());
    }
    Ok(args)
}

pub async fn whoami(
    runner: &dyn CommandRunner,
    colab_bin: &Path,
    extra_envs: &[(String, String)],
    on_line: Option<LineCallback>,
) -> Result<String, CoreError> {
    let out = runner
        .run(
            colab_spec(
                colab_bin,
                vec!["whoami".to_string()],
                CLI_SHORT_TIMEOUT,
                COLAB_SHORT_REQ_TIMEOUT_SECS,
                extra_envs,
            ),
            on_line,
        )
        .await?;
    parse_whoami_output(&out)
}

pub async fn usage(
    runner: &dyn CommandRunner,
    colab_bin: &Path,
    extra_envs: &[(String, String)],
    on_line: Option<LineCallback>,
) -> Result<UsageInfo, CoreError> {
    let out = runner
        .run(
            colab_spec(
                colab_bin,
                vec!["usage".to_string()],
                CLI_SHORT_TIMEOUT,
                COLAB_SHORT_REQ_TIMEOUT_SECS,
                extra_envs,
            ),
            on_line,
        )
        .await?;
    parse_usage_output(&out)
}

pub async fn status(
    runner: &dyn CommandRunner,
    colab_bin: &Path,
    session_name: &str,
    extra_envs: &[(String, String)],
    on_line: Option<LineCallback>,
) -> Result<SessionStatusLookup, CoreError> {
    validate_session_name(session_name)?;
    let out = runner
        .run(
            colab_spec(
                colab_bin,
                vec![
                    "status".to_string(),
                    "-s".to_string(),
                    session_name.to_string(),
                ],
                CLI_SHORT_TIMEOUT,
                COLAB_SHORT_REQ_TIMEOUT_SECS,
                extra_envs,
            ),
            on_line,
        )
        .await?;
    parse_status_output(session_name, &out)
}

pub async fn sessions(
    runner: &dyn CommandRunner,
    colab_bin: &Path,
    extra_envs: &[(String, String)],
    on_line: Option<LineCallback>,
) -> Result<Vec<SessionEntry>, CoreError> {
    let out = runner
        .run(
            colab_spec(
                colab_bin,
                vec!["sessions".to_string()],
                CLI_SHORT_TIMEOUT,
                COLAB_SHORT_REQ_TIMEOUT_SECS,
                extra_envs,
            ),
            on_line,
        )
        .await?;
    parse_sessions_output(&out)
}

pub async fn new_session(
    runner: &dyn CommandRunner,
    colab_bin: &Path,
    session_name: &str,
    gpu: GpuTier,
    high_mem: bool,
    extra_envs: &[(String, String)],
    on_line: Option<LineCallback>,
) -> Result<(), CoreError> {
    let args = build_new_args(session_name, gpu, high_mem)?;
    let out = runner
        .run(
            colab_spec(
                colab_bin,
                args,
                CLI_NEW_TIMEOUT,
                COLAB_NEW_REQ_TIMEOUT_SECS,
                extra_envs,
            ),
            on_line,
        )
        .await?;
    parse_new_output(gpu, &out)
}

pub async fn open_url(
    runner: &dyn CommandRunner,
    colab_bin: &Path,
    session_name: &str,
    extra_envs: &[(String, String)],
    on_line: Option<LineCallback>,
) -> Result<(), CoreError> {
    validate_session_name(session_name)?;
    let out = runner
        .run(
            colab_spec(
                colab_bin,
                vec![
                    "url".to_string(),
                    "-s".to_string(),
                    session_name.to_string(),
                    "--open".to_string(),
                ],
                CLI_SHORT_TIMEOUT,
                COLAB_SHORT_REQ_TIMEOUT_SECS,
                extra_envs,
            ),
            on_line,
        )
        .await?;
    ensure_no_auth_error(&out)?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: "colab url".to_string(),
            exit_code: out.exit_code,
            stderr: out.stderr,
        });
    }
    Ok(())
}

pub async fn open_pay(
    runner: &dyn CommandRunner,
    colab_bin: &Path,
    extra_envs: &[(String, String)],
    on_line: Option<LineCallback>,
) -> Result<(), CoreError> {
    let out = runner
        .run(
            colab_spec(
                colab_bin,
                vec!["pay".to_string()],
                CLI_SHORT_TIMEOUT,
                COLAB_SHORT_REQ_TIMEOUT_SECS,
                extra_envs,
            ),
            on_line,
        )
        .await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: "colab pay".to_string(),
            exit_code: out.exit_code,
            stderr: out.stderr,
        });
    }
    Ok(())
}

/// Runs `colab stop -s <name>` and checks all 3 stop verification criteria plus `colab usage`.
pub async fn stop_and_verify(
    runner: &dyn CommandRunner,
    colab_bin: &Path,
    session_name: &str,
    owner_email: &str,
    expected_endpoint_sha256: Option<&str>,
    extra_envs: &[(String, String)],
    on_line: Option<LineCallback>,
) -> Result<StopReport, CoreError> {
    validate_session_name(session_name)?;
    let stop_out = runner
        .run(
            colab_spec(
                colab_bin,
                vec![
                    "stop".to_string(),
                    "-s".to_string(),
                    session_name.to_string(),
                ],
                CLI_STOP_TIMEOUT,
                COLAB_SHORT_REQ_TIMEOUT_SECS,
                extra_envs,
            ),
            on_line.clone(),
        )
        .await?;
    ensure_no_auth_error(&stop_out)?;

    let terminated_sentinel = stop_out
        .stdout_raw
        .lines()
        .any(|l| l.trim() == "[colab] Session terminated.");

    let mut warnings = Vec::new();
    if !terminated_sentinel {
        warnings.push(format!(
            "colab stop 未返回 `[colab] Session terminated.`：{}",
            stop_out.stdout.trim()
        ));
    }

    let owner_verified = match whoami(runner, colab_bin, extra_envs, on_line.clone()).await {
        Ok(actual) => {
            if actual.eq_ignore_ascii_case(owner_email.trim()) {
                true
            } else {
                warnings.push(format!(
                    "当前 colab 账号已切换为 {actual}（开机账号 {owner_email}），无法核对原账号状态。"
                ));
                false
            }
        }
        Err(err) => {
            warnings.push(format!("无法核对 colab whoami：{err}"));
            false
        }
    };

    let endpoint_absent = if !owner_verified {
        false
    } else {
        match sessions(runner, colab_bin, extra_envs, on_line.clone()).await {
            Ok(list) => match expected_endpoint_sha256 {
                Some(hash) => {
                    let still_present = list.iter().any(|s| s.endpoint_sha256 == hash);
                    if still_present {
                        warnings.push("colab sessions 中仍检测到本会话对应实例，请立即前往 Colab 网页确认停机！".to_string());
                    }
                    !still_present
                }
                None => !list.iter().any(|s| s.name == session_name),
            },
            Err(err) => {
                warnings.push(format!("无法核对 colab sessions：{err}"));
                false
            }
        }
    };

    let usage_res = if owner_verified {
        match usage(runner, colab_bin, extra_envs, on_line).await {
            Ok(u) => Some(u),
            Err(err) => {
                warnings.push(format!("未能核对 colab usage 速率：{err}"));
                None
            }
        }
    } else {
        None
    };

    let billing_verified_stopped;
    let summary = if terminated_sentinel && endpoint_absent && owner_verified {
        match &usage_res {
            Some(u) if u.active_assignments == 0 && u.usage_rate_hourly == 0.0 => {
                billing_verified_stopped = true;
                format!(
                    "已停机并核验停止计费（Active assignments: 0，Usage rate: {:.2}/hr）。",
                    u.usage_rate_hourly
                )
            }
            Some(u) if u.active_assignments == 0 => {
                billing_verified_stopped = false;
                warnings.push(format!(
                    "Active assignments 为 0，但 Usage rate 仍为 {:.2}/hr，请到 Colab 网页核对。",
                    u.usage_rate_hourly
                ));
                format!(
                    "本会话已终止，但计算速率尚未归零（{:.2}/hr）。",
                    u.usage_rate_hourly
                )
            }
            Some(u) => {
                billing_verified_stopped = false;
                let msg = format!(
                    "本会话已停，账号下仍有 {} 个其他实例在计费（当前速率 {:.2}/hr）。",
                    u.active_assignments, u.usage_rate_hourly
                );
                warnings.push(msg.clone());
                msg
            }
            None => {
                billing_verified_stopped = false;
                "本会话已终止且实例列表已消失，但未能核对到 colab usage 速率。".to_string()
            }
        }
    } else {
        billing_verified_stopped = false;
        "未能完成全部停机核验，请勿假设已停止计费，建议立即打开 Colab 网页确认。".to_string()
    };

    Ok(StopReport {
        session_name: session_name.to_string(),
        terminated_sentinel,
        endpoint_absent,
        owner_verified,
        usage: usage_res,
        billing_verified_stopped,
        summary,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::BoxFuture;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::Mutex;

    struct FakeCall {
        expected_subcmd: String,
        output: Result<CommandOutput, CoreError>,
    }

    struct FakeRunner {
        calls: Mutex<VecDeque<FakeCall>>,
        seen_specs: Mutex<Vec<CommandSpec>>,
    }

    impl FakeRunner {
        fn new(calls: Vec<FakeCall>) -> Self {
            Self {
                calls: Mutex::new(calls.into()),
                seen_specs: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run<'a>(
            &'a self,
            spec: CommandSpec,
            _on_line: Option<LineCallback>,
        ) -> BoxFuture<'a, Result<CommandOutput, CoreError>> {
            Box::pin(async move {
                self.seen_specs.lock().unwrap().push(spec.clone());
                let next = self
                    .calls
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected command call");
                assert_eq!(
                    spec.args.first().map(String::as_str),
                    Some(next.expected_subcmd.as_str())
                );
                assert!(matches!(spec.stdin, StdinPolicy::Null));
                assert!(spec
                    .envs
                    .iter()
                    .any(|(k, v)| k == "PYTHONUNBUFFERED" && v == "1"));
                let expected_req_timeout = if next.expected_subcmd == "new" {
                    "120"
                } else {
                    "30"
                };
                assert!(spec
                    .envs
                    .iter()
                    .any(|(k, v)| k == "COLAB_CLI_REQUEST_TIMEOUT" && v == expected_req_timeout));
                next.output
            })
        }
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    const UPDATE_BANNER: &str = "\n[colab] A new version of Colab CLI is available: 0.9.0 (current: 0.8.0)\n[colab] You can run 'colab update --install' to upgrade in place.\n[colab] Run 'uv tool install -U google-colab-cli' to update.\n[colab] To silence this check, set \"enable_update_check\": false in ~/.config/colab-cli/settings.json\n\n";

    #[test]
    fn parsers_skip_daily_update_banner_and_catch_exit0_failures() {
        let status_out = CommandOutput::from_raw(
            Some(0),
            format!(
                "{UPDATE_BANNER}[comfy-1234abcd] m-s-secret123 | Hardware: NVIDIA L4 | Shape: Standard | Variant: GPU | Status: IDLE"
            ),
            String::new(),
        );
        let SessionStatusLookup::Present(entry) =
            parse_status_output("comfy-1234abcd", &status_out).unwrap()
        else {
            panic!("expected Present");
        };
        assert_eq!(entry.hardware, "NVIDIA L4");
        assert_eq!(entry.endpoint_sha256, sha256_hex(b"m-s-secret123"));

        // `colab status` exits 0 when session is missing!
        let missing_out = CommandOutput::from_raw(
            Some(0),
            format!("{UPDATE_BANNER}[colab] Session 'comfy-1234abcd' not found."),
            String::new(),
        );
        assert_eq!(
            parse_status_output("comfy-1234abcd", &missing_out).unwrap(),
            SessionStatusLookup::Missing
        );

        let usage_out = CommandOutput::from_raw(
            Some(0),
            format!(
                "{UPDATE_BANNER}Current balance: 42.50 compute units\nUsage rate: 1.85/hr\nActive assignments: 1"
            ),
            String::new(),
        );
        let usage_info = parse_usage_output(&usage_out).unwrap();
        assert_eq!(usage_info.current_balance, 42.50);
        assert_eq!(usage_info.usage_rate_hourly, 1.85);
        assert_eq!(usage_info.active_assignments, 1);
    }

    #[test]
    fn auth_and_new_error_classification_and_l4_high_mem_omission() {
        let l4_args = build_new_args("comfy-1234abcd", GpuTier::L4, true).unwrap();
        assert!(!l4_args.contains(&"--high-mem".to_string()));
        let a100_args = build_new_args("comfy-1234abcd", GpuTier::A100, true).unwrap();
        assert!(a100_args.contains(&"--high-mem".to_string()));

        let auth_out = CommandOutput::from_raw(
            Some(1),
            String::new(),
            "To authorize colab-cli, visit this URL in any browser:\n  https://accounts.google.com/o/oauth2/auth\n".to_string(),
        );
        assert!(matches!(
            parse_whoami_output(&auth_out),
            Err(CoreError::AuthRequired { .. })
        ));

        let err_400 = CommandOutput::from_raw(
            Some(1),
            "[colab] Creating session 'comfy-1234abcd'...".to_string(),
            "[colab] Backend rejected accelerator 'A100'. You may not have quota...".to_string(),
        );
        assert!(matches!(
            parse_new_output(GpuTier::A100, &err_400),
            Err(CoreError::NoAcceleratorQuota { .. })
        ));

        let err_412 = CommandOutput::from_raw(
            Some(1),
            String::new(),
            "[colab] Allocation refused (precondition failed). This can mean too many active sessions...".to_string(),
        );
        assert!(matches!(
            parse_new_output(GpuTier::L4, &err_412),
            Err(CoreError::CapacityOrSessionLimit { .. })
        ));
    }

    #[test]
    fn sessions_parses_orphan_marker() {
        let out = CommandOutput::from_raw(
            Some(0),
            "[?] m-s-orphan1 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU\n[comfy-1234abcd] m-s-mine | Hardware: NVIDIA L4 | Shape: Standard | Variant: GPU".to_string(),
            String::new(),
        );
        let list = parse_sessions_output(&out).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].is_orphan);
        assert_eq!(list[0].name, "?");
        assert!(!list[1].is_orphan);
    }

    #[test]
    fn stop_and_verify_handles_clean_stop_and_other_active_assignments() {
        let endpoint_hash = sha256_hex(b"m-s-mine");
        let runner_clean = FakeRunner::new(vec![
            FakeCall {
                expected_subcmd: "stop".to_string(),
                output: Ok(CommandOutput::from_raw(
                    Some(0),
                    "[colab] Stopping session 'comfy-1234abcd'...\n[colab] Session terminated.".to_string(),
                    String::new(),
                )),
            },
            FakeCall {
                expected_subcmd: "whoami".to_string(),
                output: Ok(CommandOutput::from_raw(
                    Some(0),
                    "Auth provider: oauth2\nEmail:         alice@example.com\n".to_string(),
                    String::new(),
                )),
            },
            FakeCall {
                expected_subcmd: "sessions".to_string(),
                output: Ok(CommandOutput::from_raw(
                    Some(0),
                    "[colab] No active sessions found on server.".to_string(),
                    String::new(),
                )),
            },
            FakeCall {
                expected_subcmd: "usage".to_string(),
                output: Ok(CommandOutput::from_raw(
                    Some(0),
                    "Current balance: 99.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                    String::new(),
                )),
            },
        ]);

        let report_clean = block_on(stop_and_verify(
            &runner_clean,
            Path::new("/usr/local/bin/colab"),
            "comfy-1234abcd",
            "alice@example.com",
            Some(&endpoint_hash),
            &[],
            None,
        ))
        .unwrap();
        assert!(report_clean.billing_verified_stopped);
        assert!(report_clean.warnings.is_empty());

        // Case 2: stop prints `not found` with exit code 0!
        let runner_missing_stop = FakeRunner::new(vec![
            FakeCall {
                expected_subcmd: "stop".to_string(),
                output: Ok(CommandOutput::from_raw(
                    Some(0),
                    "[colab] Session 'comfy-1234abcd' not found.".to_string(),
                    String::new(),
                )),
            },
            FakeCall {
                expected_subcmd: "whoami".to_string(),
                output: Ok(CommandOutput::from_raw(
                    Some(0),
                    "Email: alice@example.com\n".to_string(),
                    String::new(),
                )),
            },
            FakeCall {
                expected_subcmd: "sessions".to_string(),
                output: Ok(CommandOutput::from_raw(
                    Some(0),
                    "[?] m-s-mine | Hardware: NVIDIA L4 | Shape: Standard | Variant: GPU".to_string(),
                    String::new(),
                )),
            },
            FakeCall {
                expected_subcmd: "usage".to_string(),
                output: Ok(CommandOutput::from_raw(
                    Some(0),
                    "Current balance: 99.00 compute units\nUsage rate: 1.97/hr\nActive assignments: 1".to_string(),
                    String::new(),
                )),
            },
        ]);
        let report_fail = block_on(stop_and_verify(
            &runner_missing_stop,
            Path::new("/usr/local/bin/colab"),
            "comfy-1234abcd",
            "alice@example.com",
            Some(&endpoint_hash),
            &[],
            None,
        ))
        .unwrap();
        assert!(!report_fail.terminated_sentinel);
        assert!(!report_fail.endpoint_absent);
        assert!(!report_fail.billing_verified_stopped);
    }

    #[test]
    fn new_session_uses_120s_request_timeout_and_unbuffered_python() {
        let runner = FakeRunner::new(vec![FakeCall {
            expected_subcmd: "new".to_string(),
            output: Ok(CommandOutput::from_raw(
                Some(0),
                "[colab] Session READY.\n".to_string(),
                String::new(),
            )),
        }]);
        block_on(new_session(
            &runner,
            Path::new("/usr/local/bin/colab"),
            "comfy-1234abcd",
            GpuTier::L4,
            false,
            &[],
            None,
        ))
        .unwrap();
    }
}
