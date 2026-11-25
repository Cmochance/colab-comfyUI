use std::env;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use colab_comfy_core::colab_cli;
use colab_comfy_core::config::GpuTier;
use colab_comfy_core::locate::find_colab;
use colab_comfy_core::runner::{StdinPolicy, TokioCommandRunner};
use colab_comfy_core::SessionManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(fut)
}

struct LiveSessionGuard {
    mgr: Arc<SessionManager>,
    colab_bin: PathBuf,
    session_name: String,
    data_dir: PathBuf,
    stopped: bool,
}

impl Drop for LiveSessionGuard {
    fn drop(&mut self) {
        if !self.stopped {
            let mgr = self.mgr.clone();
            let colab_bin = self.colab_bin.clone();
            let session_name = self.session_name.clone();
            block_on(async move {
                match mgr.stop_session().await {
                    Ok(report) => {
                        eprintln!(
                            "[COLAB_LIVE][guard] stop_session summary: {} (safe_to_clear={})",
                            report.summary, report.safe_to_clear_ledger
                        );
                    }
                    Err(err) => {
                        eprintln!(
                            "[COLAB_LIVE][guard] mgr.stop_session failed ({err}); falling back to colab stop..."
                        );
                        if let Ok(email) =
                            colab_cli::whoami(&TokioCommandRunner, &colab_bin, &[], None).await
                        {
                            let _ = colab_cli::stop_and_verify(
                                &TokioCommandRunner,
                                &colab_bin,
                                &session_name,
                                &email,
                                None,
                                &[],
                                None,
                            )
                            .await;
                        }
                    }
                }
                if let Ok(usage) =
                    colab_cli::usage(&TokioCommandRunner, &colab_bin, &[], None).await
                {
                    eprintln!(
                        "[COLAB_LIVE][guard] post-cleanup usage: active_assignments={}, usage_rate={:.2}/hr",
                        usage.active_assignments, usage.usage_rate_hourly
                    );
                }
            });
        }
        let _ = fs::remove_dir_all(&self.data_dir);
    }
}

#[test]
fn live_session_lifecycle_mux_and_stop() {
    if env::var("COLAB_LIVE").as_deref() != Ok("1") {
        eprintln!("Skipping COLAB_LIVE test (set COLAB_LIVE=1 to run)");
        return;
    }

    let pid = std::process::id();
    let data_dir = PathBuf::from(format!("/tmp/ccu-live-{pid}"));
    let _ = fs::remove_dir_all(&data_dir);
    fs::create_dir_all(&data_dir).unwrap();

    let home = env::var_os("HOME").map(PathBuf::from);
    let path_env = env::var_os("PATH");
    let colab_bin = find_colab(path_env.as_deref(), home.as_deref())
        .expect("colab binary must be installed for COLAB_LIVE=1");

    // Isolate from user's running app port 18188 and session name by using 18288 + comfy-live-<pid>.
    let live_port: u16 = 18288;
    let live_session_name = format!("comfy-live-{}", pid % 100_000);
    let https_proxy = env::var_os("HTTPS_PROXY").or_else(|| env::var_os("https_proxy"));
    let http_proxy = env::var_os("HTTP_PROXY").or_else(|| env::var_os("http_proxy"));
    let mgr = Arc::new(
        SessionManager::with_deps(
            data_dir.clone(),
            home,
            path_env,
            live_port,
            Arc::new(TokioCommandRunner),
            Arc::new(colab_comfy_core::ssh_tunnel::TokioTunnelSpawner),
        )
        .with_proxy_hints(https_proxy, http_proxy)
        .with_session_name_override(live_session_name.clone())
        .expect("valid live session name"),
    );

    let mut guard = LiveSessionGuard {
        mgr: mgr.clone(),
        colab_bin: colab_bin.clone(),
        session_name: live_session_name.clone(),
        data_dir,
        stopped: false,
    };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(async {
            // H5: explicitly disable Drive so this non-interactive live test never blocks on Drive OAuth.
            let _ = mgr
                .update_preferences(None, Some(false), Some(false), None, None)
                .await?;
            eprintln!("[COLAB_LIVE] starting T4 session '{live_session_name}' (--no-drive)...");
            let snap = mgr.start_session(GpuTier::T4, false, true).await?;
            let session_name = snap
                .active_session
                .as_ref()
                .expect("active session after start")
                .session_name
                .clone();
            eprintln!("[COLAB_LIVE] session '{session_name}' ready, testing ControlMaster mux...");

            // 1. Test ControlMaster multiplexing: run 3 concurrent probes over -S <ctl>
            // without causing HTTP 429 on single-slot plans.
            let h1 = tokio::spawn({
                let m = mgr.clone();
                async move {
                    m.exec_over_mux(
                        "echo MUX_PROBE_1",
                        StdinPolicy::Null,
                        Duration::from_secs(15),
                    )
                    .await
                }
            });
            let h2 = tokio::spawn({
                let m = mgr.clone();
                async move {
                    m.exec_over_mux(
                        "echo MUX_PROBE_2",
                        StdinPolicy::Null,
                        Duration::from_secs(15),
                    )
                    .await
                }
            });
            let h3 = tokio::spawn({
                let m = mgr.clone();
                async move {
                    m.exec_over_mux("nvidia-smi -L", StdinPolicy::Null, Duration::from_secs(15))
                        .await
                }
            });
            let r1 = h1.await.expect("probe 1 join")?;
            let r2 = h2.await.expect("probe 2 join")?;
            let r3 = h3.await.expect("probe 3 join")?;
            assert_eq!(r1.1.trim(), "MUX_PROBE_1");
            assert_eq!(r2.1.trim(), "MUX_PROBE_2");
            assert_eq!(r3.0, Some(0));
            eprintln!("[COLAB_LIVE] 3 concurrent ControlMaster mux probes passed");

            // 2. Test local_proxy Host enforcement + /system_stats roundtrip
            let mut ok_stream = TcpStream::connect(("127.0.0.1", live_port)).await.unwrap();
            ok_stream
                .write_all(
                    format!("GET /system_stats HTTP/1.1\r\nHost: 127.0.0.1:{live_port}\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut ok_resp = String::new();
            ok_stream.read_to_string(&mut ok_resp).await.unwrap();
            assert!(ok_resp.starts_with("HTTP/1.1 200"));

            let mut evil_stream = TcpStream::connect(("127.0.0.1", live_port)).await.unwrap();
            evil_stream
                .write_all(
                    format!("GET /system_stats HTTP/1.1\r\nHost: evil.rebind:{live_port}\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut evil_resp = String::new();
            evil_stream.read_to_string(&mut evil_resp).await.unwrap();
            assert!(evil_resp.starts_with("HTTP/1.1 403"));
            eprintln!("[COLAB_LIVE] local_proxy /system_stats (200) and Host check (403) passed");

            // 3. Stop and verify billing stopped
            let stop_report = mgr.stop_session().await?;
            assert!(stop_report.owner_verified, "owner verification failed");
            assert!(stop_report.terminated_sentinel, "stop sentinel missing");
            assert!(
                stop_report.endpoint_absent,
                "endpoint still present after stop"
            );
            assert!(
                stop_report.safe_to_clear_ledger,
                "ledger must be safe to clear"
            );

            let usage = colab_cli::usage(&TokioCommandRunner, &colab_bin, &[], None).await?;
            eprintln!(
                "[COLAB_LIVE] stop verified for {session_name}: active_assignments={}, usage_rate={:.2}/hr",
                usage.active_assignments, usage.usage_rate_hourly
            );
            if usage.active_assignments == 0 {
                assert_eq!(
                    usage.usage_rate_hourly, 0.0,
                    "usage rate must be 0.00/hr when no active assignments"
                );
            }
            Ok::<(), colab_comfy_core::CoreError>(())
        })
    }));

    match outcome {
        Ok(Ok(())) => {
            guard.stopped = true;
        }
        Ok(Err(err)) => {
            drop(guard);
            panic!("COLAB_LIVE test failed: {err}");
        }
        Err(payload) => {
            drop(guard);
            std::panic::resume_unwind(payload);
        }
    }
}

/// Optional live Drive round-trip test (requires both `COLAB_LIVE=1` and `COLAB_LIVE_DRIVE=1`,
/// because `colab drivemount` may require interactive browser OAuth when not already mounted).
#[test]
fn live_drive_persistence_roundtrip() {
    if env::var("COLAB_LIVE").ok().as_deref() != Some("1")
        || env::var("COLAB_LIVE_DRIVE").ok().as_deref() != Some("1")
    {
        eprintln!("skipping live_drive_persistence_roundtrip (set COLAB_LIVE=1 COLAB_LIVE_DRIVE=1 to run)");
        return;
    }

    let path_env = env::var_os("PATH");
    let home = env::var_os("HOME").map(PathBuf::from);
    let Some(colab_bin) = find_colab(path_env.as_deref(), home.as_deref()) else {
        panic!("COLAB_LIVE=1 requires `colab` binary installed");
    };

    let dir = env::temp_dir().join(format!("colab-comfy-live-drive-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let data_dir = dir.join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let live_port: u16 = 18289;
    let live_session_name = format!("comfy-live-drv-{}", std::process::id() % 10000);
    let https_proxy = env::var_os("HTTPS_PROXY").or_else(|| env::var_os("https_proxy"));
    let http_proxy = env::var_os("HTTP_PROXY").or_else(|| env::var_os("http_proxy"));
    let mgr = Arc::new(
        SessionManager::with_deps(
            data_dir.clone(),
            home,
            path_env,
            live_port,
            Arc::new(TokioCommandRunner),
            Arc::new(colab_comfy_core::ssh_tunnel::TokioTunnelSpawner),
        )
        .with_proxy_hints(https_proxy, http_proxy)
        .with_session_name_override(live_session_name.clone())
        .expect("valid live session name"),
    );

    let mut guard = LiveSessionGuard {
        mgr: mgr.clone(),
        colab_bin: colab_bin.clone(),
        session_name: live_session_name.clone(),
        data_dir,
        stopped: false,
    };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(async {
            let _ = mgr
                .update_preferences(None, Some(true), Some(true), None, None)
                .await?;
            eprintln!("[COLAB_LIVE_DRIVE] starting T4 session '{live_session_name}' with Drive...");
            let mgr_start = mgr.clone();
            let start_handle =
                tokio::spawn(
                    async move { mgr_start.start_session(GpuTier::T4, false, false).await },
                );

            // Poll snapshot: if Drive OAuth prompt appears, print URL for operator & auto-continue if already authorized.
            let mut printed_url = false;
            loop {
                if start_handle.is_finished() {
                    break;
                }
                let snap = mgr.snapshot().await;
                if let Some(ref da) = snap.drive_auth {
                    if !printed_url {
                        if let Some(ref url) = da.auth_url {
                            eprintln!("[COLAB_LIVE_DRIVE] OAuth URL (complete in browser if needed): {url}");
                        }
                        printed_url = true;
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            let snap = start_handle.await.expect("start_session join")?;
            assert!(
                snap.active_session
                    .as_ref()
                    .is_some_and(|s| s.drive_enabled),
                "Drive should be enabled after mount"
            );

            // H2: manual export should succeed WITHOUT unmounting Drive
            let exp = mgr.export_to_drive_now(true).await?;
            assert!(exp.ok && exp.flush_ok && !exp.unmounted);

            // Stop & verify billing=0 + Drive flush_and_unmount
            let stop_report = mgr.stop_session().await?;
            assert!(stop_report.safe_to_clear_ledger);
            assert_eq!(stop_report.drive_sync_confirmed, Some(true));
            Ok::<(), colab_comfy_core::CoreError>(())
        })
    }));

    match outcome {
        Ok(Ok(())) => {
            guard.stopped = true;
        }
        Ok(Err(err)) => {
            drop(guard);
            panic!("COLAB_LIVE_DRIVE test failed: {err}");
        }
        Err(payload) => {
            drop(guard);
            std::panic::resume_unwind(payload);
        }
    }
}
