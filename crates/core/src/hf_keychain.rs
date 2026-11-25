use std::io::Read;
use std::time::Duration;

use crate::runner::{CommandRunner, CommandSpec, StdinPolicy};
use crate::CoreError;

pub const SECURITY_BIN: &str = "/usr/bin/security";
pub const HF_KEYCHAIN_SERVICE: &str = "io.github.cmochance.colab-comfyui";
pub const HF_KEYCHAIN_ACCOUNT: &str = "hf_token";
pub const GATEWAY_KEYCHAIN_ACCOUNT: &str = "gateway_api_key";

const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

fn build_security_interactive_set_cmd(account: &str, token: &str) -> Result<String, CoreError> {
    let trimmed = token.trim();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\'' | '\\'))
    {
        return Err(CoreError::InvalidInput {
            detail: "密钥仅允许无空白、无引号的可见 ASCII 字符".to_string(),
        });
    }
    Ok(format!(
        "add-generic-password -U -a \"{account}\" -s \"{HF_KEYCHAIN_SERVICE}\" -w \"{trimmed}\"\n"
    ))
}

async fn set_keychain_secret(
    account: &str,
    secret: &str,
    runner: &dyn CommandRunner,
) -> Result<(), CoreError> {
    let script = build_security_interactive_set_cmd(account, secret)?;
    let mut spec = CommandSpec::new(
        SECURITY_BIN.to_string(),
        vec!["-i".to_string()],
        KEYCHAIN_TIMEOUT,
    );
    spec.stdin = StdinPolicy::Bytes(script.into_bytes());
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: "security -i add-generic-password".to_string(),
            exit_code: out.exit_code,
            stderr: format!("写入 {account} 到钥匙串失败"),
        });
    }
    Ok(())
}

async fn get_keychain_secret(
    account: &str,
    runner: &dyn CommandRunner,
) -> Result<Option<String>, CoreError> {
    let spec = CommandSpec::new(
        SECURITY_BIN.to_string(),
        vec![
            "find-generic-password".to_string(),
            "-a".to_string(),
            account.to_string(),
            "-s".to_string(),
            HF_KEYCHAIN_SERVICE.to_string(),
            "-w".to_string(),
        ],
        KEYCHAIN_TIMEOUT,
    );
    let out = runner.run(spec, None).await?;
    if out.exit_code == Some(0) {
        let tok = out.stdout_raw.trim().to_string();
        if tok.is_empty() {
            return Ok(None);
        }
        return Ok(Some(tok));
    }
    // exit code 44 == errSecItemNotFound
    if out.exit_code == Some(44)
        || out.stderr.contains("could not be found")
        || out.stderr.contains("SecKeychainSearchCopyNext")
    {
        return Ok(None);
    }
    Err(CoreError::CommandFailed {
        command: "security find-generic-password".to_string(),
        exit_code: out.exit_code,
        stderr: out.stderr,
    })
}

async fn delete_keychain_secret(
    account: &str,
    runner: &dyn CommandRunner,
) -> Result<(), CoreError> {
    let spec = CommandSpec::new(
        SECURITY_BIN.to_string(),
        vec![
            "delete-generic-password".to_string(),
            "-a".to_string(),
            account.to_string(),
            "-s".to_string(),
            HF_KEYCHAIN_SERVICE.to_string(),
        ],
        KEYCHAIN_TIMEOUT,
    );
    let out = runner.run(spec, None).await?;
    if out.exit_code == Some(0)
        || out.exit_code == Some(44)
        || out.stderr.contains("could not be found")
    {
        return Ok(());
    }
    Err(CoreError::CommandFailed {
        command: "security delete-generic-password".to_string(),
        exit_code: out.exit_code,
        stderr: out.stderr,
    })
}

/// Stores `hf_token` in macOS Keychain via `/usr/bin/security -i` stdin so the token
/// never appears in process `argv`.
pub async fn set_hf_token(token: &str, runner: &dyn CommandRunner) -> Result<(), CoreError> {
    set_keychain_secret(HF_KEYCHAIN_ACCOUNT, token, runner).await
}

/// Reads `hf_token` from macOS Keychain (`None` if not stored).
pub async fn get_hf_token(runner: &dyn CommandRunner) -> Result<Option<String>, CoreError> {
    get_keychain_secret(HF_KEYCHAIN_ACCOUNT, runner).await
}

/// Checks whether `hf_token` is present in macOS Keychain without returning the secret.
pub async fn has_hf_token(runner: &dyn CommandRunner) -> bool {
    matches!(get_hf_token(runner).await, Ok(Some(_)))
}

/// Deletes `hf_token` from macOS Keychain (idempotent when missing).
pub async fn delete_hf_token(runner: &dyn CommandRunner) -> Result<(), CoreError> {
    delete_keychain_secret(HF_KEYCHAIN_ACCOUNT, runner).await
}

/// Generates a random local API key (`ccu_` + 32 hex chars) using `/dev/urandom`.
pub fn generate_gateway_api_key() -> Result<String, CoreError> {
    let mut buf = [0u8; 16];
    let mut f = std::fs::File::open("/dev/urandom").map_err(|source| CoreError::Io {
        context: "open /dev/urandom".to_string(),
        source,
    })?;
    f.read_exact(&mut buf).map_err(|source| CoreError::Io {
        context: "read /dev/urandom".to_string(),
        source,
    })?;
    Ok(format!("ccu_{}", hex::encode(buf)))
}

pub async fn set_gateway_api_key(key: &str, runner: &dyn CommandRunner) -> Result<(), CoreError> {
    set_keychain_secret(GATEWAY_KEYCHAIN_ACCOUNT, key, runner).await
}

pub async fn get_gateway_api_key(runner: &dyn CommandRunner) -> Result<Option<String>, CoreError> {
    get_keychain_secret(GATEWAY_KEYCHAIN_ACCOUNT, runner).await
}

pub async fn has_gateway_api_key(runner: &dyn CommandRunner) -> bool {
    matches!(get_gateway_api_key(runner).await, Ok(Some(_)))
}

pub async fn delete_gateway_api_key(runner: &dyn CommandRunner) -> Result<(), CoreError> {
    delete_keychain_secret(GATEWAY_KEYCHAIN_ACCOUNT, runner).await
}

pub async fn ensure_gateway_api_key(runner: &dyn CommandRunner) -> Result<String, CoreError> {
    if let Some(existing) = get_gateway_api_key(runner).await? {
        return Ok(existing);
    }
    let key = generate_gateway_api_key()?;
    set_gateway_api_key(&key, runner).await?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, LineCallback};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    struct CaptureRunner {
        seen_args: Mutex<Vec<Vec<String>>>,
        seen_stdin: Mutex<Vec<String>>,
    }

    impl CommandRunner for CaptureRunner {
        fn run<'a>(
            &'a self,
            spec: CommandSpec,
            _on_line: Option<LineCallback>,
        ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CoreError>> + Send + 'a>> {
            self.seen_args.lock().unwrap().push(spec.args.clone());
            if let StdinPolicy::Bytes(b) = &spec.stdin {
                self.seen_stdin
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(b).to_string());
            }
            Box::pin(async move {
                Ok(CommandOutput::from_raw(
                    Some(0),
                    String::new(),
                    String::new(),
                ))
            })
        }
    }

    #[test]
    fn set_hf_token_uses_security_interactive_stdin_and_never_puts_token_in_argv() {
        let runner = CaptureRunner {
            seen_args: Mutex::new(Vec::new()),
            seen_stdin: Mutex::new(Vec::new()),
        };
        let secret = "hf_super_secret_token_12345";
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(set_hf_token(secret, &runner)).unwrap();

        let args = runner.seen_args.lock().unwrap();
        assert_eq!(args[0], vec!["-i"]);
        assert!(!args[0].iter().any(|a| a.contains(secret)));

        let stdin = runner.seen_stdin.lock().unwrap();
        assert!(stdin[0].contains(
            "add-generic-password -U -a \"hf_token\" -s \"io.github.cmochance.colab-comfyui\""
        ));
        assert!(stdin[0].contains(secret));
    }

    #[test]
    fn set_hf_token_rejects_quotes_whitespace_and_backslash() {
        for bad in ["hf_bad\"quote", "hf_bad space", "hf_bad\\slash", ""] {
            assert!(
                build_security_interactive_set_cmd(HF_KEYCHAIN_ACCOUNT, bad).is_err(),
                "expected error for {bad:?}"
            );
        }
    }

    #[test]
    fn gateway_api_key_generates_ccu_prefix_and_stores_via_stdin() {
        let key = generate_gateway_api_key().unwrap();
        assert!(key.starts_with("ccu_"));
        assert_eq!(key.len(), 36);
        let runner = CaptureRunner {
            seen_args: Mutex::new(Vec::new()),
            seen_stdin: Mutex::new(Vec::new()),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(set_gateway_api_key(&key, &runner)).unwrap();
        let stdin = runner.seen_stdin.lock().unwrap();
        assert!(stdin[0].contains("add-generic-password -U -a \"gateway_api_key\""));
        assert!(stdin[0].contains(&key));
    }
}
