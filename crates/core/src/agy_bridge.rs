use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::locate::OPEN_BIN;
use crate::runner::{CommandRunner, CommandSpec};
use crate::CoreError;

const AGY_LS_TIMEOUT: Duration = Duration::from_secs(15);
const AGY_SWITCH_TIMEOUT: Duration = Duration::from_secs(30);
const AGY_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgyColabAccount {
    pub name: String,
    pub email: String,
    pub balance_cu: Option<f64>,
    pub usage_rate_hourly: Option<f64>,
    pub active_assignments: Option<u32>,
    pub is_current: bool,
}

/// Parses `agy-cs colab ls` and `agy-cs colab refresh quota` output lines:
/// `  alysechencn   alysechencn@gmail.com   397.47 CU   0.00/hr  0 active`
/// `* mochancechen  mochancechen@gmail.com  2500.00 CU  0.00/hr  0 active`
pub fn parse_agy_colab_ls(stdout: &str) -> Vec<AgyColabAccount> {
    let mut accounts = Vec::new();
    for raw_line in stdout.lines() {
        let trimmed_start = raw_line.trim_start();
        if trimmed_start.is_empty() {
            continue;
        }
        let (is_current, rest) = if let Some(after_star) = trimmed_start.strip_prefix('*') {
            (true, after_star.trim_start())
        } else {
            (false, trimmed_start)
        };
        let cols: Vec<&str> = rest.split_whitespace().collect();
        if cols.len() < 2 {
            continue;
        }
        let name = cols[0];
        let email = cols[1];
        if name.is_empty() || !email.contains('@') {
            continue;
        }

        let mut balance_cu = None;
        let mut usage_rate_hourly = None;
        let mut active_assignments = None;
        let tail = &cols[2..];
        for (i, token) in tail.iter().enumerate() {
            if *token == "CU" && i > 0 {
                if let Ok(val) = tail[i - 1].parse::<f64>() {
                    balance_cu = Some(val);
                }
            } else if let Some(num) = token.strip_suffix("/hr") {
                if let Ok(val) = num.parse::<f64>() {
                    usage_rate_hourly = Some(val);
                }
            } else if *token == "active" && i > 0 {
                if let Ok(val) = tail[i - 1].parse::<u32>() {
                    active_assignments = Some(val);
                }
            }
        }

        accounts.push(AgyColabAccount {
            name: name.to_string(),
            email: email.to_string(),
            balance_cu,
            usage_rate_hourly,
            active_assignments,
            is_current,
        });
    }
    accounts
}

/// Looks up the current `agy-cs` account name dynamically by `email` (never relies on cached aliases).
pub fn find_account_name_by_email<'a>(
    accounts: &'a [AgyColabAccount],
    email: &str,
) -> Option<&'a str> {
    let target = email.trim();
    if target.is_empty() {
        return None;
    }
    accounts
        .iter()
        .find(|acc| acc.email.eq_ignore_ascii_case(target))
        .map(|acc| acc.name.as_str())
}

pub async fn list_agy_colab_accounts(
    agy_cs_bin: &Path,
    runner: &dyn CommandRunner,
) -> Result<Vec<AgyColabAccount>, CoreError> {
    let spec = CommandSpec::new(
        agy_cs_bin.display().to_string(),
        vec!["colab".to_string(), "ls".to_string()],
        AGY_LS_TIMEOUT,
    );
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: "agy-cs colab ls".to_string(),
            exit_code: out.exit_code,
            stderr: if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            },
        });
    }
    Ok(parse_agy_colab_ls(&out.stdout_raw))
}

pub async fn refresh_agy_colab_quota(
    agy_cs_bin: &Path,
    proxy_envs: &[(String, String)],
    runner: &dyn CommandRunner,
) -> Result<Vec<AgyColabAccount>, CoreError> {
    let mut spec = CommandSpec::new(
        agy_cs_bin.display().to_string(),
        vec![
            "colab".to_string(),
            "refresh".to_string(),
            "quota".to_string(),
        ],
        AGY_REFRESH_TIMEOUT,
    );
    spec.envs = proxy_envs.to_vec();
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: "agy-cs colab refresh quota".to_string(),
            exit_code: out.exit_code,
            stderr: if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            },
        });
    }
    Ok(parse_agy_colab_ls(&out.stdout_raw))
}

pub async fn switch_agy_colab_account(
    agy_cs_bin: &Path,
    name: &str,
    runner: &dyn CommandRunner,
) -> Result<(), CoreError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return Err(CoreError::InvalidInput {
            detail: format!("invalid agy-cs account name: '{name}'"),
        });
    }
    let spec = CommandSpec::new(
        agy_cs_bin.display().to_string(),
        vec![
            "colab".to_string(),
            "switch".to_string(),
            trimmed.to_string(),
        ],
        AGY_SWITCH_TIMEOUT,
    );
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: format!("agy-cs colab switch {trimmed}"),
            exit_code: out.exit_code,
            stderr: if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            },
        });
    }
    Ok(())
}

pub async fn open_agy_switcher_app(
    app_path: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), CoreError> {
    let spec = CommandSpec::new(
        OPEN_BIN.to_string(),
        vec!["-a".to_string(), app_path.display().to_string()],
        AGY_LS_TIMEOUT,
    );
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: "open Agy Switcher.app".to_string(),
            exit_code: out.exit_code,
            stderr: out.stderr,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_agy_cs_colab_ls_output_and_finds_by_email() {
        let sample = "\
  alysechencn   alysechencn@gmail.com   397.47 CU   0.00/hr  0 active\n\
  mltnchristen  mltnchristen@gmail.com  200.00 CU   0.00/hr  0 active\n\
* mochancechen  mochancechen@gmail.com  2500.00 CU  1.24/hr  1 active\n";

        let accounts = parse_agy_colab_ls(sample);
        assert_eq!(accounts.len(), 3);
        assert_eq!(accounts[0].name, "alysechencn");
        assert_eq!(accounts[0].balance_cu, Some(397.47));
        assert!(!accounts[0].is_current);

        assert_eq!(accounts[2].name, "mochancechen");
        assert!(accounts[2].is_current);
        assert_eq!(accounts[2].balance_cu, Some(2500.0));
        assert_eq!(accounts[2].usage_rate_hourly, Some(1.24));
        assert_eq!(accounts[2].active_assignments, Some(1));

        assert_eq!(
            find_account_name_by_email(&accounts, "ALYSECHENCN@GMAIL.COM"),
            Some("alysechencn")
        );
        assert_eq!(
            find_account_name_by_email(&accounts, "unknown@gmail.com"),
            None
        );
    }
}
