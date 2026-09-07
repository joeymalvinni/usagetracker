# Claude Launch Credential Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Before opening a managed Claude session, re-sync that account’s OAuth into the profile Keychain Claude Code reads; on failure, start Reconnect instead of Terminal — per `docs/superpowers/specs/2026-07-31-claude-launch-credential-sync-design.md`.

**Architecture:** Add `credentials::sync_for_launch` that loads (Keychain → file fallback), refreshes via an injected async refresher when `is_expired()`, always writes the credential JSON to the **profile Keychain service** with `set_password_if_changed`, then invalidates the broker cache. `ClaudeAdapter::launch` calls it for managed profiles before writing the launcher; failures call the existing repair/login path and return without `open_terminal`.

**Tech Stack:** Rust (`usage-daemon`), existing `keychain` broker, `ClaudeApiClient::refresh_credentials`. Verification: `cargo test -p usage-daemon`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`.

## Global Constraints

- Never put tokens in the `.command` launcher, logs, socket payloads, or UI copy.
- Sync only the selected account’s credentials into that account’s Keychain service (`keychain_service_for_config_dir`).
- Do not copy from the legacy global `Claude Code-credentials` item into a managed profile.
- No `.credentials.json` dual-write; no `CLAUDE_CODE_OAUTH_TOKEN` in the launcher.
- Fixture mode: launch rejected; no Keychain writes (existing fixture gate stays first).
- Commit style: imperative sentence, no conventional-commit prefix.

---

## File Structure

**Modified:**
- `crates/usage-daemon/src/providers/claude/credentials.rs` — `sync_for_launch` + unit tests (pure / injectable)
- `crates/usage-daemon/src/providers/claude/adapter.rs` — call sync in `LaunchHandler::launch`; reconnect on failure
- `docs/claude.md` — short launch-sync note

**Unchanged:** `providers/launchers.rs` (launcher must remain token-free)

---

### Task 1: `sync_for_launch` helper + unit tests

**Files:**
- Modify: `crates/usage-daemon/src/providers/claude/credentials.rs`
- Test: inline in same file

**Interfaces:**
- Produces:
```rust
pub(super) async fn sync_for_launch<F, Fut>(
    keychain_service: String,
    keychain_account: String,
    credentials_file_path: PathBuf,
    refresh: F,
) -> Result<(), ProviderError>
where
    F: FnOnce(ClaudeCredentials) -> Fut,
    Fut: std::future::Future<Output = Result<ClaudeCredentials, ProviderError>>,
```
- Behavior:
  1. `load_credentials(service, account, file).await?`
  2. If `credentials.is_expired()` → `credentials = refresh(credentials).await?` (refresher is responsible for persisting via existing `refresh_credentials` / `save_credentials`)
  3. Regardless of Keychain vs File source, write `credentials.source_contents()` (or `raw` serialized the same way Keychain stores today — use the string that `save_keychain` expects, i.e. after refresh the updated `source_contents` from save, or `credentials.raw.to_string()` consistently with `save_credentials` Keychain arm which uses `credentials.raw.to_string()`) to Keychain via `keychain::set_password_if_changed(service, account, contents)`.
  4. `keychain::invalidate_password_cache(service, account)?` (map errors like other Keychain failures).
  5. Return `Ok(())`. Load/refresh/write failures propagate as `ProviderError` (caller maps to reconnect).

**Important:** `save_credentials` writes back to the *source*. Sync step 3 must target Keychain explicitly so a file-sourced load still seeds the profile Keychain item Claude reads.

- [ ] **Step 1: Write failing tests** in `credentials.rs` `mod tests`:

Use a small test double — prefer testing the orchestration with `#[cfg(test)]` hooks **or** test only the pure decision + a `sync_for_launch` test that uses unique keychain service names if the macOS keychain helper works in tests.

Minimum required tests that do not need live Keychain if you extract internals:

```rust
#[test]
fn expired_credentials_are_detected_with_skew() {
    let mut credentials = parse_credentials(
        r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1}}"#,
        "svc",
        "acct",
        CredentialSource::Keychain,
    )
    .unwrap();
    assert!(credentials.is_expired());
    // far-future expiresAt
    credentials = parse_credentials(
        &format!(
            r#"{{"claudeAiOauth":{{"accessToken":"a","refreshToken":"r","expiresAt":{}}}}}"#,
            chrono::Utc::now().timestamp_millis() + 3_600_000
        ),
        "svc",
        "acct",
        CredentialSource::Keychain,
    )
    .unwrap();
    assert!(!credentials.is_expired());
}
```

And an async test for `sync_for_launch` that:
- Uses a unique service name `format!("usage-tracker-test-sync-{}", uuid::Uuid::new_v4())`
- `set_password_if_changed` / write a known JSON blob first (or let sync load from a temp credentials file when Keychain missing)
- Calls `sync_for_launch` with a refresh closure that **panics if called** when not expired
- Asserts Keychain get returns the same JSON (presence), then `delete_password` cleanup

And a second async test where expiresAt is in the past, refresh closure returns updated credentials (and records it was called), sync writes the refreshed contents.

If live Keychain is flaky in CI/sandbox, gate Keychain round-trips with `#[cfg(target_os = "macos")]` and still keep the expired/refresh-invocation test by injecting a `LoadSync` trait — prefer the unique-service Keychain tests first; they match production.

- [ ] **Step 2: Run tests — expect FAIL** (function missing)

`cargo test -p usage-daemon sync_for_launch expired_credentials -- --nocapture 2>&1 | tail -30`

- [ ] **Step 3: Implement `sync_for_launch`**

Follow the Interfaces block. Map Keychain errors with existing `keychain_load_error` / `keychain_save_error` patterns. Do not log token contents.

After a successful Keychain write (or no-op from `set_if_changed`), always `invalidate_password_cache`.

- [ ] **Step 4: Tests PASS**

`cargo test -p usage-daemon sync_for_launch expired_credentials 2>&1 | tail -40`

- [ ] **Step 5: Commit**

```bash
git add crates/usage-daemon/src/providers/claude/credentials.rs
git commit -m "$(cat <<'EOF'
Add Claude launch credential Keychain sync helper

EOF
)"
```

---

### Task 2: Wire sync + reconnect into `ClaudeAdapter::launch`

**Files:**
- Modify: `crates/usage-daemon/src/providers/claude/adapter.rs`
- Modify: `docs/claude.md` (short subsection)

**Interfaces:**
- Consumes: `credentials::sync_for_launch`, `ClaudeApiClient::refresh_credentials`, `prepare_login_profile` / `launch_claude_login` / `monitor_login` (same as `RepairHandler::repair`)
- In `launch`, after resolving managed `config_dir` + `saved`, **before** `write_claude_profile_launcher`:

```rust
if let Some(config_dir) = config_dir.as_ref() {
    let service = saved
        .keychain_service
        .clone()
        .unwrap_or_else(|| keychain_service_for_config_dir(config_dir));
    let account = saved
        .keychain_account
        .clone()
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "default".to_string()));
    let credentials_file = saved
        .credentials_file
        .clone()
        .map(expand_home_path)
        .unwrap_or_else(|| config_dir.join(".credentials.json"));

    let api = ClaudeApiClient::new(HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT)?;
    if let Err(err) = credentials::sync_for_launch(
        service,
        account,
        credentials_file,
        |creds| api.refresh_credentials(creds),
    )
    .await
    {
        // Reconnect instead of opening a session.
        let target = prepare_login_profile(runtime, Some(&account.id)).await?;
        let login = launchers::launch_claude_login(target.config_dir.as_deref())?;
        let authentication_url = login.authentication_url.clone();
        launchers::monitor_login(
            login.child,
            runtime.refresh(),
            PROVIDER_ID,
            Some(target.profile_id),
        );
        return Ok(ProviderActionResponse {
            provider_id: account.provider_id,
            message: format!(
                "Claude credentials need reconnect before opening a session ({}). Finish signing in in your browser.",
                err.kind().as_str()
            ),
            authentication_url,
        });
    }
}
```

Ensure:
- Message does **not** include token material (kind/code only).
- Legacy path with `config_dir == None` skips sync.
- Fixture gate in `DaemonRuntime::launch_provider_account` still runs first (no sync in fixture).
- Success path unchanged after sync (launcher + open + persist prefs).

Export `sync_for_launch` / `credentials` module access: `credentials` is already `mod` in `claude/mod.rs` — use `super::credentials` or `crate::providers::claude::credentials` from adapter (adapter is in same crate module tree — check existing imports; may need `use super::credentials` and make `sync_for_launch` `pub(super)`).

- [ ] **Step 1: Failing tests** in `adapter.rs` tests (or `daemon.rs` if launch is easier there):

Prefer a focused unit test of a small extracted helper if full launch needs Terminal:

```rust
#[tokio::test]
async fn launch_credential_sync_failure_message_mentions_reconnect_not_tokens() {
    // Construct a ProviderError::CredentialsMissing and format the same
    // message builder used by launch (extract `fn reconnect_after_sync_failure(err: &ProviderError) -> String`)
    let msg = reconnect_after_sync_failure(&ProviderError::new(
        ProviderErrorKind::CredentialsMissing,
        "missing",
    ));
    assert!(msg.to_lowercase().contains("reconnect"));
    assert!(!msg.contains("sk-"));
    assert!(!msg.contains("accessToken"));
}
```

Plus keep an integration-style test if one already opens launch with mocks — otherwise document that Task 1 covers sync mechanics and this task covers wiring + message helper.

Optional stronger test: refactor launch’s post-sync failure arm into `async fn start_reconnect_instead_of_session(...)` and assert it returns `authentication_url` option path without calling open_terminal — only if low-cost.

- [ ] **Step 2: Implement wiring + `reconnect_after_sync_failure` helper**

- [ ] **Step 3: Update `docs/claude.md`**

Add under security / managed accounts:

> Before opening a managed profile session, UsageTracker re-syncs that account’s OAuth into the profile’s Keychain item Claude Code reads. If credentials cannot be loaded or refreshed, Open Session starts Reconnect instead of Terminal.

- [ ] **Step 4: Run tests**

```bash
cargo test -p usage-daemon sync_for_launch reconnect_after_sync launch_credential 2>&1 | tail -40
cargo test -p usage-daemon launch_ 2>&1 | tail -40
```

- [ ] **Step 5: Commit**

```bash
git add crates/usage-daemon/src/providers/claude/adapter.rs docs/claude.md
git commit -m "$(cat <<'EOF'
Sync Claude OAuth into profile Keychain before open session

EOF
)"
```

---

### Task 3: Verification gate

- [ ] **Step 1: Full daemon + clippy/fmt**

```bash
cargo test -p usage-daemon 2>&1 | tail -50
cargo clippy --all-targets -- -D warnings 2>&1 | tail -40
cargo fmt --all -- --check
```

- [ ] **Step 2: Spec checklist**
  - Always sync managed launch ✅
  - Refresh when expired ✅
  - Failure → Reconnect, no Terminal ✅
  - No tokens in launcher ✅
  - Fixture unchanged ✅
  - Docs note ✅

- [ ] **Step 3: Fix fallout if any; commit only if needed**

---

## Spec coverage

| Spec item | Task |
| --- | --- |
| Keychain-only sync | 1 |
| Always re-sync | 1–2 |
| Refresh near expiry | 1 |
| Failure → Reconnect | 2 |
| No tokens in launcher | 2 (unchanged launchers) |
| Docs | 2 |
| Tests | 1–3 |
