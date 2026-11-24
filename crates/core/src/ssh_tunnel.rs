use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

use crate::colab_cli::{self, SessionStatusLookup};
use crate::config::{
    active_session_path, comfy_socket_path, ctl_socket_path, load_json_opt, posix_single_quote,
    ssh_key_path, ssh_pub_key_path, validate_session_name, ActiveSession, StateLock,
};
use crate::locate::{SSH_BIN, SSH_KEYGEN_BIN};
use crate::runner::{BoxFuture, CommandRunner, CommandSpec, LineCallback, StdinPolicy};
use crate::CoreError;

const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const KEYGEN_TIMEOUT: Duration = Duration::from_secs(15);
const MASTER_READY_TIMEOUT: Duration = Duration::from_secs(45);
const MUX_CTL_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle for a running `ssh -M -S ...` ControlMaster process (real or mocked).
pub trait TunnelProcess: Send {
    fn try_wait(&mut self) -> Result<Option<i32>, CoreError>;
    fn kill<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>>;
}

pub struct TokioTunnelProcess {
    child: Child,
}

impl TunnelProcess for TokioTunnelProcess {
    fn try_wait(&mut self) -> Result<Option<i32>, CoreError> {
        self.child
            .try_wait()
            .map(|opt| opt.map(|status| status.code().unwrap_or(-1)))
            .map_err(|source| CoreError::Io {
                context: "ssh try_wait".to_string(),
                source,
            })
    }

    fn kill<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>> {
        Box::pin(async move {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
            Ok(())
        })
    }
}

/// Spawns the `ssh -M` ControlMaster child process.
pub trait TunnelSpawner: Send + Sync {
    fn spawn(&self, program: &Path, args: &[String]) -> Result<Box<dyn TunnelProcess>, CoreError>;
}

#[derive(Debug, Default)]
pub struct TokioTunnelSpawner;

impl TunnelSpawner for TokioTunnelSpawner {
    fn spawn(&self, program: &Path, args: &[String]) -> Result<Box<dyn TunnelProcess>, CoreError> {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| CoreError::Io {
                context: format!("spawn {}", program.display()),
                source,
            })?;
        Ok(Box::new(TokioTunnelProcess { child }))
    }
}

/// Ensures `~/.colab-comfyui/ssh/id_ed25519` (+ `.pub`) exists with `0600` permissions.
pub async fn ensure_ed25519_key(
    data_dir: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), CoreError> {
    let key_path = ssh_key_path(data_dir);
    let pub_path = ssh_pub_key_path(data_dir);

    if key_path.exists() && pub_path.exists() {
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            CoreError::Io {
                context: format!("chmod 0600 {}", key_path.display()),
                source,
            }
        })?;
        return Ok(());
    }

    if key_path.exists() {
        let _ = fs::remove_file(&key_path);
    }
    if pub_path.exists() {
        let _ = fs::remove_file(&pub_path);
    }

    let spec = CommandSpec::new(
        SSH_KEYGEN_BIN,
        vec![
            "-t".to_string(),
            "ed25519".to_string(),
            "-N".to_string(),
            String::new(),
            "-C".to_string(),
            "colab-comfyui".to_string(),
            "-f".to_string(),
            key_path.display().to_string(),
        ],
        KEYGEN_TIMEOUT,
    );
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) || !key_path.exists() {
        return Err(CoreError::CommandFailed {
            command: SSH_KEYGEN_BIN.to_string(),
            exit_code: out.exit_code,
            stderr: out.stderr,
        });
    }
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        CoreError::Io {
            context: format!("chmod 0600 {}", key_path.display()),
            source,
        }
    })?;
    Ok(())
}

/// Builds the exact non-interactive OpenSSH `ControlMaster` arguments forwarding
/// `127.0.0.1:8188` on Colab to `<data_dir>/run/comfy.sock`.
pub fn build_master_ssh_args(
    colab_bin: &Path,
    key_path: &Path,
    ctl_sock: &Path,
    comfy_sock: &Path,
    session_name: &str,
) -> Result<Vec<String>, CoreError> {
    validate_session_name(session_name)?;
    let proxy_cmd = format!(
        "{} ssh --proxy-mode -i {} -s {}",
        posix_single_quote(&colab_bin.display().to_string()),
        posix_single_quote(&key_path.display().to_string()),
        posix_single_quote(session_name),
    );
    Ok(vec![
        "-M".to_string(),
        "-S".to_string(),
        ctl_sock.display().to_string(),
        "-N".to_string(),
        "-F".to_string(),
        "/dev/null".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=20".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-o".to_string(),
        "StreamLocalBindUnlink=yes".to_string(),
        "-o".to_string(),
        "StreamLocalBindMask=0177".to_string(),
        "-o".to_string(),
        format!("ProxyCommand={proxy_cmd}"),
        "-i".to_string(),
        key_path.display().to_string(),
        "-L".to_string(),
        format!("{}:127.0.0.1:8188", comfy_sock.display()),
        "root@colab-runtime".to_string(),
    ])
}

/// Executes a remote command strictly over the existing `ControlMaster` socket.
/// Never triggers `colab ssh --proxy-mode`.
pub fn build_mux_exec_spec(
    ctl_sock: &Path,
    remote_cmd: &str,
    stdin: StdinPolicy,
    timeout: Duration,
) -> CommandSpec {
    let mut spec = CommandSpec::new(
        SSH_BIN,
        vec![
            "-S".to_string(),
            ctl_sock.display().to_string(),
            "-F".to_string(),
            "/dev/null".to_string(),
            "-o".to_string(),
            "ControlMaster=no".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "root@colab-runtime".to_string(),
            "--".to_string(),
            remote_cmd.to_string(),
        ],
        timeout,
    );
    spec.stdin = stdin;
    spec
}

pub fn build_mux_check_spec(ctl_sock: &Path) -> CommandSpec {
    CommandSpec::new(
        SSH_BIN,
        vec![
            "-S".to_string(),
            ctl_sock.display().to_string(),
            "-F".to_string(),
            "/dev/null".to_string(),
            "-O".to_string(),
            "check".to_string(),
            "root@colab-runtime".to_string(),
        ],
        MUX_CTL_TIMEOUT,
    )
}

pub fn build_mux_exit_spec(ctl_sock: &Path) -> CommandSpec {
    CommandSpec::new(
        SSH_BIN,
        vec![
            "-S".to_string(),
            ctl_sock.display().to_string(),
            "-F".to_string(),
            "/dev/null".to_string(),
            "-O".to_string(),
            "exit".to_string(),
            "root@colab-runtime".to_string(),
        ],
        MUX_CTL_TIMEOUT,
    )
}

/// Triple gate inside `StateLock` before any SSH master start/reconnect:
/// 1) `active-session.json` on disk matches `expected`;
/// 2) `colab whoami` matches `expected.owner_email`;
/// 3) `colab status -s <session>` shows `[<session>]` with `Hardware:` and matching `endpoint_sha256`.
///
/// Returns pre-connect `colab sessions` count so post-connect checks detect ghost instances.
pub async fn verify_tunnel_preconditions(
    data_dir: &Path,
    colab_bin: &Path,
    expected: &ActiveSession,
    runner: &dyn CommandRunner,
    on_line: Option<LineCallback>,
) -> Result<usize, CoreError> {
    let _lock = StateLock::acquire(data_dir, LOCK_TIMEOUT).await?;

    let disk_session: Option<ActiveSession> = load_json_opt(&active_session_path(data_dir))?;
    match disk_session {
        Some(ref s) if s == expected => {}
        Some(ref s) => {
            return Err(CoreError::SessionLedgerMismatch {
                detail: format!(
                    "expected {} ({}), found {} ({})",
                    expected.session_name, expected.owner_email, s.session_name, s.owner_email
                ),
            });
        }
        None => {
            return Err(CoreError::SessionLedgerMismatch {
                detail: "active-session.json is missing".to_string(),
            });
        }
    }

    let actual_email = colab_cli::whoami(runner, colab_bin, on_line.clone()).await?;
    if !actual_email.eq_ignore_ascii_case(expected.owner_email.trim()) {
        return Err(CoreError::AccountMismatch {
            expected_email: expected.owner_email.clone(),
            actual_email,
        });
    }

    match colab_cli::status(runner, colab_bin, &expected.session_name, on_line.clone()).await? {
        SessionStatusLookup::Present(entry) => {
            if entry.endpoint_sha256 != expected.endpoint_sha256 {
                return Err(CoreError::SessionLedgerMismatch {
                    detail: "session endpoint fingerprint changed on Colab server".to_string(),
                });
            }
        }
        SessionStatusLookup::Missing => {
            return Err(CoreError::SessionMissing {
                session_name: expected.session_name.clone(),
            });
        }
    }

    let pre_sessions = colab_cli::sessions(runner, colab_bin, on_line).await?;
    Ok(pre_sessions.len())
}

pub struct StartedMasterTunnel {
    pub process: Box<dyn TunnelProcess>,
    pub ctl_sock: std::path::PathBuf,
    pub comfy_sock: std::path::PathBuf,
    pub session_count_warning: Option<String>,
}

/// Runs the Triple Gate, spawns `ssh -M -S <ctl>`, waits for `-O check`,
/// and verifies `colab sessions` count did not increase.
pub async fn start_master_and_verify(
    data_dir: &Path,
    colab_bin: &Path,
    session: &ActiveSession,
    runner: &dyn CommandRunner,
    spawner: &dyn TunnelSpawner,
    on_line: Option<LineCallback>,
) -> Result<StartedMasterTunnel, CoreError> {
    let pre_count =
        verify_tunnel_preconditions(data_dir, colab_bin, session, runner, on_line.clone()).await?;
    ensure_ed25519_key(data_dir, runner).await?;

    let ctl_sock = ctl_socket_path(data_dir, &session.session_name)?;
    let comfy_sock = comfy_socket_path(data_dir)?;
    let _ = fs::remove_file(&ctl_sock);
    let _ = fs::remove_file(&comfy_sock);

    let key_path = ssh_key_path(data_dir);
    let args = build_master_ssh_args(
        colab_bin,
        &key_path,
        &ctl_sock,
        &comfy_sock,
        &session.session_name,
    )?;

    let mut process = spawner.spawn(Path::new(SSH_BIN), &args)?;

    let deadline = tokio::time::Instant::now() + MASTER_READY_TIMEOUT;
    loop {
        if let Some(code) = process.try_wait()? {
            return Err(CoreError::TunnelFailed {
                detail: format!("ssh ControlMaster exited early with code {code}"),
            });
        }
        let check = runner.run(build_mux_check_spec(&ctl_sock), None).await;
        if let Ok(out) = check {
            if out.exit_code == Some(0) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = process.kill().await;
            return Err(CoreError::TunnelFailed {
                detail: "timed out waiting for SSH ControlMaster socket".to_string(),
            });
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let post_count = colab_cli::sessions(runner, colab_bin, on_line)
        .await
        .map(|v| v.len())
        .unwrap_or(pre_count);
    let session_count_warning = if post_count > pre_count {
        Some(format!(
            "警告：隧道重连后账号下活跃实例数从 {pre_count} 增至 {post_count}，请立即检查 Colab 会话管理！"
        ))
    } else {
        None
    };

    Ok(StartedMasterTunnel {
        process,
        ctl_sock,
        comfy_sock,
        session_count_warning,
    })
}

pub async fn stop_master_tunnel(
    ctl_sock: &Path,
    comfy_sock: &Path,
    process: Option<&mut Box<dyn TunnelProcess>>,
    runner: &dyn CommandRunner,
) {
    let _ = runner.run(build_mux_exit_spec(ctl_sock), None).await;
    if let Some(proc) = process {
        let _ = proc.kill().await;
    }
    let _ = fs::remove_file(ctl_sock);
    let _ = fs::remove_file(comfy_sock);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ensure_data_dirs, save_json_0600, sha256_hex, GpuTier, LOCAL_PROXY_PORT};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct ShortTempDir(PathBuf);

    impl ShortTempDir {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = PathBuf::from(format!(
                "/tmp/ccu-tun-{}-{}",
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

    struct MockProc;
    impl TunnelProcess for MockProc {
        fn try_wait(&mut self) -> Result<Option<i32>, CoreError> {
            Ok(None)
        }
        fn kill<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct MockSpawner {
        spawn_count: AtomicUsize,
        last_args: Mutex<Vec<String>>,
    }

    impl MockSpawner {
        fn new() -> Self {
            Self {
                spawn_count: AtomicUsize::new(0),
                last_args: Mutex::new(Vec::new()),
            }
        }
    }

    impl TunnelSpawner for MockSpawner {
        fn spawn(
            &self,
            _program: &Path,
            args: &[String],
        ) -> Result<Box<dyn TunnelProcess>, CoreError> {
            self.spawn_count.fetch_add(1, Ordering::SeqCst);
            *self.last_args.lock().unwrap() = args.to_vec();
            Ok(Box::new(MockProc))
        }
    }

    struct ScriptedRunner {
        replies: Mutex<VecDeque<crate::runner::CommandOutput>>,
    }

    impl ScriptedRunner {
        fn new(replies: Vec<crate::runner::CommandOutput>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
            }
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run<'a>(
            &'a self,
            _spec: CommandSpec,
            _on_line: Option<LineCallback>,
        ) -> BoxFuture<'a, Result<crate::runner::CommandOutput, CoreError>> {
            Box::pin(async move {
                Ok(self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("missing scripted reply"))
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

    #[test]
    fn master_args_include_controlmaster_non_interactive_flags_and_escaped_proxy_command() {
        let args = build_master_ssh_args(
            Path::new("/Users/a b/colab"),
            Path::new("/tmp/ccu/ssh/id_ed25519"),
            Path::new("/tmp/ccu/ssh/ctl-12345678"),
            Path::new("/tmp/ccu/run/comfy.sock"),
            "comfy-1234abcd",
        )
        .unwrap();

        for required in [
            "-M",
            "-N",
            "BatchMode=yes",
            "ExitOnForwardFailure=yes",
            "StrictHostKeyChecking=no",
            "UserKnownHostsFile=/dev/null",
            "IdentitiesOnly=yes",
            "LogLevel=ERROR",
            "ServerAliveInterval=20",
            "ServerAliveCountMax=3",
            "/tmp/ccu/run/comfy.sock:127.0.0.1:8188",
            "root@colab-runtime",
        ] {
            assert!(args.contains(&required.to_string()), "missing {required}");
        }
        assert!(args.iter().any(|a| a.contains("ProxyCommand='/Users/a b/colab' ssh --proxy-mode -i '/tmp/ccu/ssh/id_ed25519' -s 'comfy-1234abcd'")));
    }

    #[test]
    fn triple_gate_aborts_ssh_spawn_on_account_mismatch_or_missing_session() {
        let tmp = ShortTempDir::new();
        ensure_data_dirs(&tmp.0).unwrap();
        fs::write(ssh_key_path(&tmp.0), "priv").unwrap();
        fs::write(ssh_pub_key_path(&tmp.0), "pub").unwrap();

        let session = ActiveSession {
            session_name: "comfy-1234abcd".to_string(),
            owner_email: "alice@example.com".to_string(),
            endpoint_sha256: sha256_hex(b"m-s-mine"),
            gpu: GpuTier::L4,
            high_mem: false,
            hardware_summary: "NVIDIA L4".to_string(),
            local_port: LOCAL_PROXY_PORT,
            created_at_unix: 1,
        };
        save_json_0600(&active_session_path(&tmp.0), &session).unwrap();

        // Gate 1: whoami changed to bob@example.com
        let runner_mismatch = ScriptedRunner::new(vec![crate::runner::CommandOutput::from_raw(
            Some(0),
            "Email: bob@example.com\n".to_string(),
            String::new(),
        )]);
        let spawner = MockSpawner::new();
        let err = block_on(start_master_and_verify(
            &tmp.0,
            Path::new("/usr/local/bin/colab"),
            &session,
            &runner_mismatch,
            &spawner,
            None,
        ))
        .err()
        .unwrap();
        assert!(matches!(err, CoreError::AccountMismatch { .. }));
        assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 0);

        // Gate 2: status reports session missing
        let runner_missing = ScriptedRunner::new(vec![
            crate::runner::CommandOutput::from_raw(
                Some(0),
                "Email: alice@example.com\n".to_string(),
                String::new(),
            ),
            crate::runner::CommandOutput::from_raw(
                Some(0),
                "[colab] Session 'comfy-1234abcd' not found.".to_string(),
                String::new(),
            ),
        ]);
        let err2 = block_on(start_master_and_verify(
            &tmp.0,
            Path::new("/usr/local/bin/colab"),
            &session,
            &runner_missing,
            &spawner,
            None,
        ))
        .err()
        .unwrap();
        assert!(matches!(err2, CoreError::SessionMissing { .. }));
        assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn start_master_warns_if_session_count_increases_after_connect() {
        let tmp = ShortTempDir::new();
        ensure_data_dirs(&tmp.0).unwrap();
        fs::write(ssh_key_path(&tmp.0), "priv").unwrap();
        fs::write(ssh_pub_key_path(&tmp.0), "pub").unwrap();

        let session = ActiveSession {
            session_name: "comfy-1234abcd".to_string(),
            owner_email: "alice@example.com".to_string(),
            endpoint_sha256: sha256_hex(b"m-s-mine"),
            gpu: GpuTier::L4,
            high_mem: false,
            hardware_summary: "NVIDIA L4".to_string(),
            local_port: LOCAL_PROXY_PORT,
            created_at_unix: 1,
        };
        save_json_0600(&active_session_path(&tmp.0), &session).unwrap();

        let runner = ScriptedRunner::new(vec![
            // whoami
            crate::runner::CommandOutput::from_raw(
                Some(0),
                "Email: alice@example.com\n".to_string(),
                String::new(),
            ),
            // status
            crate::runner::CommandOutput::from_raw(
                Some(0),
                "[comfy-1234abcd] m-s-mine | Hardware: NVIDIA L4 | Shape: Standard | Variant: GPU".to_string(),
                String::new(),
            ),
            // pre sessions: 1
            crate::runner::CommandOutput::from_raw(
                Some(0),
                "[comfy-1234abcd] m-s-mine | Hardware: NVIDIA L4 | Shape: Standard | Variant: GPU".to_string(),
                String::new(),
            ),
            // ssh -O check
            crate::runner::CommandOutput::from_raw(Some(0), String::new(), String::new()),
            // post sessions: 2 (ghost detected!)
            crate::runner::CommandOutput::from_raw(
                Some(0),
                "[comfy-1234abcd] m-s-mine | Hardware: NVIDIA L4 | Shape: Standard | Variant: GPU\n[?] m-s-ghost | Hardware: CPU | Shape: Standard | Variant: DEFAULT".to_string(),
                String::new(),
            ),
        ]);

        let spawner = MockSpawner::new();
        let started = block_on(start_master_and_verify(
            &tmp.0,
            Path::new("/usr/local/bin/colab"),
            &session,
            &runner,
            &spawner,
            None,
        ))
        .unwrap();
        assert_eq!(spawner.spawn_count.load(Ordering::SeqCst), 1);
        assert!(started.session_count_warning.is_some());
    }
}
