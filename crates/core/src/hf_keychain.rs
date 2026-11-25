use std::time::Duration;

use crate::runner::{CommandRunner, CommandSpec, StdinPolicy};
use crate::CoreError;

pub const SECURITY_BIN: &str = "/usr/bin/security";
pub const HF_KEYCHAIN_SERVICE: &str = "io.github.cmochance.colab-comfyui";
pub const HF_KEYCHAIN_ACCOUNT: &str = "hf_token";

const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

fn build_security_interactive_set_cmd(token: &str) -> Result<String, CoreError> {
    let trimmed = token.trim();
    if trimmed.is_empty()
        || trimmed.contains('\n')
        || trimmed.contains('\r')
        || trimmed.contains('\0')
    {
        return Err(CoreError::InvalidInput {
            detail: "HF_TOKEN 不能为空或包含换行符".to_string(),
        });
    }
    let escaped = trimmed.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        "add-generic-password -U -a \"{HF_KEYCHAIN_ACCOUNT}\" -s \"{HF_KEYCHAIN_SERVICE}\" -w \"{escaped}\"\n"
    ))
}

/// Stores `hf_token` in macOS Keychain via `/usr/bin/security -i` stdin so the token
/// never appears in process `argv`.
pub async fn set_hf_token(token: &str, runner: &dyn CommandRunner) -> Result<(), CoreError> {
    let script = build_security_interactive_set_cmd(token)?;
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
            stderr: "写入 Hugging Face Token 到钥匙串失败".to_string(),
        });
    }
    Ok(())
}

/// Reads `hf_token` from macOS Keychain (`None` if not stored).
pub async fn get_hf_token(runner: &dyn CommandRunner) -> Result<Option<String>, CoreError> {
    let spec = CommandSpec::new(
        SECURITY_BIN.to_string(),
        vec![
            "find-generic-password".to_string(),
            "-a".to_string(),
            HF_KEYCHAIN_ACCOUNT.to_string(),
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

/// Checks whether `hf_token` is present in macOS Keychain without returning the secret.
pub async fn has_hf_token(runner: &dyn CommandRunner) -> bool {
    matches!(get_hf_token(runner).await, Ok(Some(_)))
}

/// Deletes `hf_token` from macOS Keychain (idempotent when missing).
pub async fn delete_hf_token(runner: &dyn CommandRunner) -> Result<(), CoreError> {
    let spec = CommandSpec::new(
        SECURITY_BIN.to_string(),
        vec![
            "delete-generic-password".to_string(),
            "-a".to_string(),
            HF_KEYCHAIN_ACCOUNT.to_string(),
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
}
