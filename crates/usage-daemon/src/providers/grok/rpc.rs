//! Bounded JSON-RPC client for the official `grok agent stdio` ACP surface.

use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::providers::{ProviderError, ProviderErrorKind};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(4);
const BILLING_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_STDOUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 64 * 1024;
const BINARY_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(2);

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn find_grok_binary() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os("GROK_CLI_PATH").filter(|path| !path.is_empty()) {
        return executable_file(PathBuf::from(override_path));
    }
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".grok/bin/grok"));
        candidates.push(home.join(".local/bin/grok"));
    }
    candidates.extend([
        PathBuf::from("/usr/local/bin/grok"),
        PathBuf::from("/opt/homebrew/bin/grok"),
    ]);
    if let Some(path) = resolve_from_login_shell() {
        candidates.push(path);
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("grok")));
    }
    candidates.into_iter().find_map(executable_file)
}

fn executable_file(path: PathBuf) -> Option<PathBuf> {
    let metadata = path.metadata().ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(path)
}

fn resolve_from_login_shell() -> Option<PathBuf> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args([
            "-lic",
            "type -P grok 2>/dev/null || whence -p grok 2>/dev/null || command -v grok",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let status = match child.wait_timeout(BINARY_RESOLUTION_TIMEOUT).ok()? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    if !status.success() {
        return None;
    }
    let mut bytes = Vec::new();
    child
        .stdout
        .take()?
        .take(4 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    let path = String::from_utf8(bytes).ok()?;
    executable_file(PathBuf::from(path.lines().next()?.trim()))
}

pub(super) fn fetch_billing(binary: &Path) -> Result<Value, ProviderError> {
    run_rpc(binary).map_err(|err| classify_rpc_error(&err.to_string()))
}

fn run_rpc(binary: &Path) -> anyhow::Result<Value> {
    let mut child = ChildGuard(
        Command::new(binary)
            .args(["--no-auto-update", "agent", "stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut stdin = child
        .0
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open Grok RPC stdin"))?;
    let stdout = child
        .0
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open Grok RPC stdout"))?;
    let stderr = child
        .0
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open Grok RPC stderr"))?;

    let (line_tx, line_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout.take(MAX_STDOUT_BYTES)).lines() {
            let stop = line.is_err();
            if line_tx.send(line).is_err() || stop {
                break;
            }
        }
    });
    let (stderr_tx, stderr_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(MAX_STDERR_BYTES).read_to_end(&mut bytes);
        let _ = stderr_tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });

    write_request(
        &mut stdin,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}
        }),
    )?;
    let initialize = read_response(&line_rx, 1, INITIALIZE_TIMEOUT, "initialize")?;
    let initialize_result = rpc_result(initialize, "initialize")?;

    if let Some(method_id) = authentication_method(&initialize_result) {
        write_request(
            &mut stdin,
            2,
            "authenticate",
            json!({"methodId":method_id,"_meta":{"headless":true}}),
        )?;
        let authenticate = read_response(&line_rx, 2, INITIALIZE_TIMEOUT, "authenticate")?;
        let _ = rpc_result(authenticate, "authenticate")?;
    }

    write_request(&mut stdin, 3, "x.ai/billing", json!({}))?;
    let billing = read_response(&line_rx, 3, BILLING_TIMEOUT, "x.ai/billing")?;
    let result = rpc_result(billing, "x.ai/billing")?;
    drop(stdin);
    let _ = child.0.kill();
    let _ = child.0.wait_timeout(Duration::from_secs(2));
    let _ = stderr_rx.recv_timeout(Duration::from_millis(100));
    Ok(result)
}

fn write_request(
    stdin: &mut impl Write,
    id: i64,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    let payload = encode_request(id, method, params)?;
    stdin.write_all(&payload)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn encode_request(id: i64, method: &str, params: Value) -> anyhow::Result<Vec<u8>> {
    let payload =
        serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
    // Some JSON encoders escape solidus characters. Grok's ACP method
    // dispatcher historically compared the encoded spelling, so normalize
    // defensively even though serde_json currently emits `/` directly.
    let mut output = Vec::with_capacity(payload.len());
    let mut index = 0;
    while index < payload.len() {
        if payload[index..].starts_with(b"\\/") {
            output.push(b'/');
            index += 2;
        } else {
            output.push(payload[index]);
            index += 1;
        }
    }
    Ok(output)
}

fn read_response(
    receiver: &mpsc::Receiver<std::io::Result<String>>,
    expected_id: i64,
    timeout: Duration,
    method: &str,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow::anyhow!("Grok RPC {method} timed out after {timeout:?}"))?;
        let line = receiver.recv_timeout(remaining).map_err(|err| match err {
            mpsc::RecvTimeoutError::Timeout => {
                anyhow::anyhow!("Grok RPC {method} timed out after {timeout:?}")
            }
            mpsc::RecvTimeoutError::Disconnected => {
                anyhow::anyhow!("Grok RPC stdout closed during {method}")
            }
        })??;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("id").and_then(Value::as_i64) == Some(expected_id) {
            return Ok(message);
        }
    }
}

fn rpc_result(message: Value, method: &str) -> anyhow::Result<Value> {
    if let Some(error) = message.get("error") {
        let detail = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        anyhow::bail!("Grok RPC {method} returned error: {detail}");
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Grok RPC {method} response omitted result"))
}

fn authentication_method(initialize: &Value) -> Option<&str> {
    let methods = initialize.get("authMethods")?.as_array()?;
    let ids = methods
        .iter()
        .filter_map(|method| method.get("id")?.as_str())
        .collect::<Vec<_>>();
    if ids.contains(&"cached_token") {
        Some("cached_token")
    } else if std::env::var_os("XAI_API_KEY").is_some() && ids.contains(&"xai.api_key") {
        Some("xai.api_key")
    } else {
        None
    }
}

fn classify_rpc_error(message: &str) -> ProviderError {
    let lower = message.to_ascii_lowercase();
    let kind = if lower.contains("rate limit") || lower.contains("too many requests") {
        ProviderErrorKind::RateLimited
    } else if lower.contains("authentication")
        || lower.contains("grok login")
        || lower.contains("authenticate")
    {
        ProviderErrorKind::CredentialsInvalid
    } else {
        ProviderErrorKind::ProviderUnavailable
    };
    ProviderError::new(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_cached_auth_before_api_key() {
        let value = json!({"authMethods":[{"id":"xai.api_key"},{"id":"cached_token"}]});
        assert_eq!(authentication_method(&value), Some("cached_token"));
    }

    #[test]
    fn classifies_method_absence_as_fallback_eligible() {
        assert_eq!(
            classify_rpc_error("-32601 Method not found").kind(),
            ProviderErrorKind::ProviderUnavailable
        );
    }

    #[test]
    fn billing_method_is_encoded_with_literal_solidus() {
        let payload = encode_request(3, "x.ai/billing", json!({})).unwrap();
        let text = String::from_utf8(payload).unwrap();
        assert!(text.contains(r#""method":"x.ai/billing""#));
        assert!(!text.contains(r#"x.ai\/billing"#));
    }

    #[test]
    fn performs_current_acp_authentication_before_billing() {
        let root = std::env::temp_dir().join(format!("grok-rpc-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("grok");
        std::fs::write(&script, r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"authMethods":[{"id":"cached_token"}]}}' ;;
    *'"method":"authenticate"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}' ;;
    *'"method":"x.ai/billing"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"monthlyLimit":{"val":100},"usage":{"includedUsed":{"val":25}}}}' ;;
  esac
done
"#).unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();

        let result = run_rpc(&script).unwrap();
        assert_eq!(result["usage"]["includedUsed"]["val"], 25);
        std::fs::remove_dir_all(root).unwrap();
    }
}
