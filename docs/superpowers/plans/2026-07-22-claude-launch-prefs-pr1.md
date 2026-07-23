# Claude Launch Prefs + Confirm-on-Open (PR1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Per-account Claude launch preferences (working directory + structured flags) with a confirm-on-open sheet, per the approved design `docs/superpowers/specs/2026-07-22-claude-launch-import-design.md` (PR1 slice — **no importer**).

**Architecture:** Additive v3 wire types (`LaunchFlags`, `get_account_launch_settings`, launch overrides) flow from the Swift Open sheet through the socket into the Claude adapter, which merges overrides with saved `ClaudeProfileSettings`, validates, writes an extended `.command` launcher (cd guard + flags), and persists prefs on successful open. A `launch_options` provider capability gates the sheet so a new app never silently sends overrides an old daemon would drop.

**Tech Stack:** Rust (tokio, serde, schemars, thiserror, anyhow) in `crates/usage-core` + `crates/usage-daemon`; Swift (SwiftUI/AppKit, XCTest, strict concurrency) in `apps/UsageMenuBar`. Verification: `cargo test --workspace`, `swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete`, `just check`.

**Repo conventions that bind this plan:**
- New Claude profile settings need **dual registration**: the `ClaudeProfileSettings` struct (`deny_unknown_fields`) AND `ClaudeAdapter::profile_setting_keys()` — otherwise `remove_unsupported_settings` silently strips and persists away the keys at next daemon load.
- Protocol is exact-match v3 with closed tagged enums. The repo's own tests pin: `supports_method`, checked-in schemas (`checked_in_protocol_schemas_are_current`), wire-fixture round-trips, and Swift fixture decoding. Every wire change lands with all of them in the same commit.
- Commit style: imperative sentence, no conventional-commit prefix (e.g. "Fix stale app process during updates").

---

## File Structure

**Modified (Rust):**
- `crates/usage-core/src/api.rs` — `LaunchEffort`, `LaunchFlags`, `AccountLaunchSettingsResponse`, request/response variants, `supports_method`, `ProviderCapabilities.launch_options`
- `crates/usage-core/wire-fixtures/account_launch_settings_v3.json` — **new** fixture
- `docs/api/schemas/v3/request.json`, `docs/api/schemas/v3/response.json` — regenerated
- `crates/usage-daemon/src/providers/claude/settings.rs` — new optional fields + `validate_launch_flags`
- `crates/usage-daemon/src/providers/claude/adapter.rs` — key allowlist + parity test, `resolve_launch_plan`, launch rewrite, `launch_settings`
- `crates/usage-daemon/src/providers/launchers.rs` — cwd + flags + cd guard in `claude_launcher_contents`
- `crates/usage-daemon/src/runtime/provider_adapter.rs` — `LaunchOverrides`, `InvalidLaunchRequest`, trait extensions, capability derivation
- `crates/usage-daemon/src/daemon.rs` — plumbing + updated launcher tests
- `crates/usage-daemon/src/server.rs` — new arm, override passthrough, `launch_failure_response`
- `docs/api/methods.md`, `docs/api/versioning.md`, `docs/claude.md`, `CHANGELOG.md`

**Modified (Swift):**
- `apps/UsageMenuBar/Sources/UsageMenuBar/Networking/Wire.swift` — request/response cases, `ProviderCapabilities.launchOptions`
- `apps/UsageMenuBar/Sources/UsageMenuBar/Networking/DaemonClient.swift` — two methods + timeout entry
- `apps/UsageMenuBar/Sources/UsageMenuBar/App/AppState.swift` — capability helper, `openSession` state, prepare/confirm
- `apps/UsageMenuBar/Sources/UsageMenuBar/Views/Settings/Settings.swift` — menu wiring
- `apps/UsageMenuBar/Tests/UsageMenuBarTests/ReliabilityTests.swift` — wire tests

**Created (Swift):**
- `apps/UsageMenuBar/Sources/UsageMenuBar/Networking/Models/AccountLaunchSettings.swift` — `LaunchFlags`, `AccountLaunchSettingsResponse`
- `apps/UsageMenuBar/Sources/UsageMenuBar/State/OpenSession.swift` — `EffortChoice`, `OpenSessionModel` (pure, testable)
- `apps/UsageMenuBar/Sources/UsageMenuBar/Views/Settings/OpenSessionSheet.swift` — sheet view + `OpenSessionWindow` presenter
- `apps/UsageMenuBar/Tests/UsageMenuBarTests/OpenSessionModelTests.swift`

---

### Task 1: usage-core wire types + variants (workspace stays green)

**Files:**
- Modify: `crates/usage-core/src/api.rs`
- Modify: `crates/usage-daemon/src/server.rs:411` (destructure + stub arm)
- Modify: `crates/usage-daemon/src/runtime/provider_adapter.rs:398-407` (capability literal)
- Regenerate: `docs/api/schemas/v3/request.json`, `docs/api/schemas/v3/response.json`

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `crates/usage-core/src/api.rs`:

```rust
    #[test]
    fn launch_request_decodes_optional_overrides_and_defaults() {
        let request: RequestEnvelope = serde_json::from_str(
            r#"{"api_version":3,"method":"launch_provider_account","account_id":"account-1","working_directory":"~/Projects/demo","launch":{"model":"fable","effort":"xhigh","dangerously_skip_permissions":true},"remember_dangerously_skip_permissions":true}"#,
        )
        .unwrap();
        let ApiRequest::LaunchProviderAccount {
            account_id,
            working_directory,
            launch,
            remember_dangerously_skip_permissions,
        } = request.request
        else {
            panic!("unexpected request variant");
        };
        assert_eq!(account_id.as_str(), "account-1");
        assert_eq!(working_directory.as_deref(), Some("~/Projects/demo"));
        let launch = launch.unwrap();
        assert_eq!(launch.model.as_deref(), Some("fable"));
        assert_eq!(launch.effort, Some(LaunchEffort::Xhigh));
        assert!(launch.dangerously_skip_permissions);
        assert!(remember_dangerously_skip_permissions);

        let bare: RequestEnvelope = serde_json::from_str(
            r#"{"api_version":3,"method":"launch_provider_account","account_id":"account-1"}"#,
        )
        .unwrap();
        let ApiRequest::LaunchProviderAccount {
            working_directory,
            launch,
            remember_dangerously_skip_permissions,
            ..
        } = bare.request
        else {
            panic!("unexpected request variant");
        };
        assert_eq!(working_directory, None);
        assert_eq!(launch, None);
        assert!(!remember_dangerously_skip_permissions);
    }

    #[test]
    fn account_launch_settings_are_supported_and_round_trip() {
        assert!(ApiRequest::supports_method("get_account_launch_settings"));

        let response = ResponseEnvelope::new(ApiResponse::AccountLaunchSettings {
            settings: AccountLaunchSettingsResponse {
                provider_id: ProviderId::new("claude"),
                account_id: AccountId::new("account-1"),
                working_directory: Some("~/Projects/demo".to_string()),
                launch: Some(LaunchFlags {
                    model: Some("fable".to_string()),
                    effort: Some(LaunchEffort::Xhigh),
                    dangerously_skip_permissions: false,
                }),
                has_managed_config_dir: true,
            },
        });
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["type"], "account_launch_settings");
        assert_eq!(value["settings"]["launch"]["effort"], "xhigh");
        // dangerously_skip_permissions is skipped when false.
        assert!(value["settings"]["launch"]
            .as_object()
            .unwrap()
            .get("dangerously_skip_permissions")
            .is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p usage-core launch_request_decodes 2>&1 | tail -5`
Expected: **compile error** — `no variant named ...` / unknown fields on `LaunchProviderAccount` (the types don't exist yet).

- [ ] **Step 3: Implement the types in `crates/usage-core/src/api.rs`.** Below `ProviderToggle` (after line ~155), add:

```rust
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl LaunchEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Structured launch flags for provider sessions. Structured on purpose —
/// values are validated and shell-quoted by the daemon; never free-form argv.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
pub struct LaunchFlags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<LaunchEffort>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dangerously_skip_permissions: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct AccountLaunchSettingsResponse {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchFlags>,
    pub has_managed_config_dir: bool,
}
```

Replace the `LaunchProviderAccount` request variant (api.rs:120-122) with:

```rust
    LaunchProviderAccount {
        account_id: AccountId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_directory: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch: Option<LaunchFlags>,
        #[serde(default, skip_serializing_if = "is_false")]
        remember_dangerously_skip_permissions: bool,
    },
    GetAccountLaunchSettings {
        account_id: AccountId,
    },
```

In `supports_method` (api.rs:126-149) add `| "get_account_launch_settings"` after `"launch_provider_account"`.

In the `ApiResponse` enum (after `ProviderAction`, api.rs:338-340) add:

```rust
    AccountLaunchSettings {
        settings: AccountLaunchSettingsResponse,
    },
```

In `ProviderCapabilities` (api.rs:404-414) add after `launch_account`:

```rust
    /// The launch handler accepts working-directory / flag overrides and
    /// exposes per-account launch settings.
    #[serde(default, skip_serializing_if = "is_false")]
    pub launch_options: bool,
```

Update the existing test `request_envelope_round_trips_with_flat_method` (api.rs:693-706): change the match arm to `ApiRequest::LaunchProviderAccount { account_id, .. }`.

- [ ] **Step 4: Keep usage-daemon compiling (staged stubs).**

In `crates/usage-daemon/src/runtime/provider_adapter.rs:398-407` add to the `ProviderCapabilities` literal in `descriptor()` (real derivation lands in Task 6):

```rust
                launch_account: self.launch_handler().is_some(),
                // Staged off until the handler advertises override support (Task 6).
                launch_options: false,
```

In `crates/usage-daemon/src/server.rs:411` change the arm head to `ApiRequest::LaunchProviderAccount { account_id, .. } =>` (body unchanged), and add a staged arm after it (real handler lands in Task 7):

```rust
            ApiRequest::GetAccountLaunchSettings { account_id } => {
                if let Some(error) = self.account_validation_error(&account_id).await {
                    error
                } else {
                    ApiResponse::error(
                        ApiErrorCode::UnsupportedOperation,
                        "account launch settings are not implemented yet",
                    )
                }
            }
```

- [ ] **Step 5: Regenerate the checked-in schemas** (the pin test `checked_in_protocol_schemas_are_current` fails until you do):

Run (from repo root): `cargo run -p usage-core --example generate-schemas`
Expected: `generated protocol v3 schemas in docs/api/schemas/v3`

- [ ] **Step 6: Run the full workspace test suite**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: PASS (all crates compile; new tests green; schema pin green). Note: `usage-cli` matches `ApiResponse` non-exhaustively with fallthroughs, so the new response variant compiles without CLI changes — if the compiler disagrees anywhere, add a `_ =>` fallthrough arm matching that match's existing error style.

- [ ] **Step 7: Commit**

```bash
git add crates/usage-core/src/api.rs crates/usage-daemon/src/server.rs crates/usage-daemon/src/runtime/provider_adapter.rs docs/api/schemas/v3
git commit -m "Add launch settings wire types to protocol v3

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Wire fixture for the new response type

**Files:**
- Create: `crates/usage-core/wire-fixtures/account_launch_settings_v3.json`
- Modify: `crates/usage-core/src/api.rs` (tests only)

- [ ] **Step 1: Create the fixture** `crates/usage-core/wire-fixtures/account_launch_settings_v3.json`:

```json
{"api_version":3,"type":"account_launch_settings","settings":{"provider_id":"claude","account_id":"account-1","working_directory":"~/Projects/demo","launch":{"model":"fable","effort":"xhigh","dangerously_skip_permissions":true},"has_managed_config_dir":true}}
```

- [ ] **Step 2: Write the failing decode test + extend the round-trip list.** In `crates/usage-core/src/api.rs` tests, add to the array inside `wire_fixtures_round_trip_without_losing_contract_fields` (api.rs:779-792):

```rust
            include_str!("../wire-fixtures/account_launch_settings_v3.json"),
```

And add a decode test:

```rust
    #[test]
    fn fixture_account_launch_settings_decodes() {
        let response: ResponseEnvelope = serde_json::from_str(include_str!(
            "../wire-fixtures/account_launch_settings_v3.json"
        ))
        .unwrap();
        let ApiResponse::AccountLaunchSettings { settings } = response.response else {
            panic!("unexpected fixture response");
        };
        assert_eq!(settings.provider_id.as_str(), "claude");
        assert_eq!(settings.launch.unwrap().effort, Some(LaunchEffort::Xhigh));
        assert!(settings.has_managed_config_dir);
    }
```

- [ ] **Step 3: Run**

Run: `cargo test -p usage-core fixture_account_launch_settings_decodes wire_fixtures_round_trip 2>&1 | tail -5`
Expected: PASS (if round-trip fails, the fixture JSON key order/whitespace must be exactly one line as above — `assert_eq!` compares `serde_json::Value`s, so only field names/values matter, not order).

- [ ] **Step 4: Commit**

```bash
git add crates/usage-core/wire-fixtures/account_launch_settings_v3.json crates/usage-core/src/api.rs
git commit -m "Pin account launch settings wire fixture

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Claude profile settings fields + validation + parity test

**Files:**
- Modify: `crates/usage-daemon/src/providers/claude/settings.rs`
- Modify: `crates/usage-daemon/src/providers/claude/adapter.rs:47-57` (keys) + tests

- [ ] **Step 1: Write the failing parity test** in `crates/usage-daemon/src/providers/claude/adapter.rs` inside `mod tests`:

```rust
    #[test]
    fn profile_setting_keys_cover_every_serialized_settings_field() {
        // Dual-registration guard: a field missing from profile_setting_keys()
        // is silently stripped and persisted away at the next config load.
        let settings = settings::ClaudeProfileSettings {
            keychain_account: Some("user".to_string()),
            keychain_service: Some("service".to_string()),
            credentials_file: Some(PathBuf::from("/tmp/credentials.json")),
            claude_config_dir: Some(PathBuf::from("/tmp/profile")),
            cli_enabled: Some(true),
            project_roots: vec![PathBuf::from("/tmp/projects")],
            owns_default_claude_activity: true,
            working_directory: Some(PathBuf::from("/tmp/work")),
            launch: Some(usage_core::LaunchFlags {
                model: Some("fable".to_string()),
                effort: Some(usage_core::LaunchEffort::Xhigh),
                dangerously_skip_permissions: true,
            }),
        };
        let value = serde_json::to_value(&settings).unwrap();
        let keys = ADAPTER.profile_setting_keys();
        for field in value.as_object().unwrap().keys() {
            assert!(
                keys.contains(&field.as_str()),
                "profile_setting_keys() is missing {field}"
            );
        }
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p usage-daemon profile_setting_keys_cover 2>&1 | tail -5`
Expected: **compile error** — `working_directory`/`launch` fields don't exist yet.

- [ ] **Step 3: Add the fields and validation.** In `crates/usage-daemon/src/providers/claude/settings.rs`, extend the struct (after `owns_default_claude_activity`, settings.rs:24-25):

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) launch: Option<usage_core::LaunchFlags>,
```

Replace `validate` (settings.rs:28-35) with:

```rust
pub(crate) fn validate(config: &ProviderConfig) -> anyhow::Result<()> {
    config.ensure_settings_empty("Claude provider")?;
    for (index, profile) in config.profiles.iter().enumerate() {
        let settings = self::profile(profile)
            .with_context(|| format!("invalid Claude profile configuration at index {index}"))?;
        if let Some(flags) = &settings.launch {
            validate_launch_flags(flags)
                .with_context(|| format!("invalid Claude launch flags at index {index}"))?;
        }
    }
    Ok(())
}

/// Effort is a closed enum at the wire layer; the model string is constrained
/// here because both end up inside a generated shell script.
pub(crate) fn validate_launch_flags(flags: &usage_core::LaunchFlags) -> anyhow::Result<()> {
    if let Some(model) = &flags.model {
        anyhow::ensure!(!model.trim().is_empty(), "launch model cannot be blank");
        anyhow::ensure!(model.len() <= 128, "launch model is too long");
        anyhow::ensure!(
            model
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "._:/-".contains(ch)),
            "launch model contains unsupported characters"
        );
    }
    Ok(())
}
```

Add validation tests in a new `#[cfg(test)] mod tests` at the bottom of settings.rs:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::LaunchFlags;

    #[test]
    fn launch_flag_validation_constrains_the_model_string() {
        let valid = LaunchFlags {
            model: Some("claude-fable-5".to_string()),
            ..LaunchFlags::default()
        };
        assert!(validate_launch_flags(&valid).is_ok());

        for model in ["", "   ", "model'; rm -rf /", "a".repeat(129).as_str()] {
            let flags = LaunchFlags {
                model: Some(model.to_string()),
                ..LaunchFlags::default()
            };
            assert!(
                validate_launch_flags(&flags).is_err(),
                "model {model:?} should be rejected"
            );
        }
    }
}
```

In `crates/usage-daemon/src/providers/claude/adapter.rs:47-57` extend `profile_setting_keys()`:

```rust
        &[
            "keychain_account",
            "keychain_service",
            "credentials_file",
            "claude_config_dir",
            "cli_enabled",
            "project_roots",
            "owns_default_claude_activity",
            "working_directory",
            "launch",
        ]
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p usage-daemon profile_setting_keys_cover launch_flag_validation 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/usage-daemon/src/providers/claude/settings.rs crates/usage-daemon/src/providers/claude/adapter.rs
git commit -m "Store Claude launch preferences in profile settings

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Launcher — working directory, flags, cd guard

**Files:**
- Modify: `crates/usage-daemon/src/providers/launchers.rs:237-283` + tests
- Modify: `crates/usage-daemon/src/providers/claude/adapter.rs:299-300` (call site)
- Modify: `crates/usage-daemon/src/daemon.rs:1301-1316` (existing launcher tests)

- [ ] **Step 1: Write the failing tests** in `crates/usage-daemon/src/providers/launchers.rs` `mod tests`:

```rust
    #[test]
    fn launcher_adds_cd_guard_and_structured_flags() {
        let flags = usage_core::LaunchFlags {
            model: Some("fable".to_string()),
            effort: Some(usage_core::LaunchEffort::Xhigh),
            dangerously_skip_permissions: true,
        };
        let contents = claude_launcher_contents(
            Some(Path::new("/tmp/profile")),
            Some(Path::new("/tmp/My Project's Code")),
            Some(&flags),
        );
        // cd failure must never fall through to $HOME with dangerous flags.
        assert!(contents.contains("cd -- '/tmp/My Project'\"'\"'s Code' || exit 1"));
        assert!(contents.ends_with(
            "exec claude --model 'fable' --effort 'xhigh' --dangerously-skip-permissions\n"
        ));
    }

    #[test]
    fn launcher_omits_missing_working_directory_and_flags() {
        let contents = claude_launcher_contents(Some(Path::new("/tmp/profile")), None, None);
        assert!(!contents.contains("cd -- "));
        assert!(contents.ends_with("exec claude\n"));

        let default_flags = usage_core::LaunchFlags::default();
        let bare = claude_launcher_contents(None, None, Some(&default_flags));
        assert!(bare.ends_with("exec claude\n"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p usage-daemon launcher_adds_cd_guard 2>&1 | tail -5`
Expected: **compile error** — `claude_launcher_contents` takes 1 argument.

- [ ] **Step 3: Implement.** Replace `write_claude_profile_launcher` and `claude_launcher_contents` (launchers.rs:237-279) with:

```rust
pub(crate) fn write_claude_profile_launcher(
    account_id: &AccountId,
    config_dir: Option<&Path>,
    working_directory: Option<&Path>,
    flags: Option<&usage_core::LaunchFlags>,
) -> anyhow::Result<PathBuf> {
    let app_dir = default_app_dir()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve ~/.usagetracker directory"))?;
    let launcher_dir = app_dir.join("launchers");
    std::fs::create_dir_all(&launcher_dir)?;
    let launcher = launcher_dir.join(format!("claude-{}.command", account_id.as_str()));
    let temporary = launcher_dir.join(format!(
        ".claude-{}.{}.tmp",
        account_id.as_str(),
        uuid::Uuid::new_v4()
    ));
    let contents = claude_launcher_contents(config_dir, working_directory, flags);
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &launcher)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(launcher)
}

pub(crate) fn claude_launcher_contents(
    config_dir: Option<&Path>,
    working_directory: Option<&Path>,
    flags: Option<&usage_core::LaunchFlags>,
) -> String {
    let profile_setup = match config_dir {
        Some(path) => format!(
            "export CLAUDE_CONFIG_DIR={}\n",
            shell_single_quote(&path.display().to_string())
        ),
        None => "unset CLAUDE_CONFIG_DIR\n".to_string(),
    };
    let change_directory = match working_directory {
        // A stale launcher re-run after the directory vanishes must stop,
        // not exec claude in $HOME with these flags.
        Some(path) => format!(
            "cd -- {} || exit 1\n",
            shell_single_quote(&path.display().to_string())
        ),
        None => String::new(),
    };
    let mut command = String::from("exec claude");
    if let Some(flags) = flags {
        if let Some(model) = &flags.model {
            command.push_str(&format!(" --model {}", shell_single_quote(model)));
        }
        if let Some(effort) = flags.effort {
            command.push_str(&format!(" --effort {}", shell_single_quote(effort.as_str())));
        }
        if flags.dangerously_skip_permissions {
            command.push_str(" --dangerously-skip-permissions");
        }
    }
    format!(
        "#!/bin/zsh -l\nunset CLAUDE_SECURESTORAGE_CONFIG_DIR\n{profile_setup}{change_directory}{command}\n"
    )
}
```

Fix the three existing call sites:
- launchers.rs test `launcher_quotes_profile_paths_and_clears_legacy_overrides` (launchers.rs:309-316): `claude_launcher_contents(Some(Path::new("/tmp/Claude's Work")), None, None)` and `claude_launcher_contents(None, None, None)`.
- `crates/usage-daemon/src/providers/claude/adapter.rs:299-300`: `launchers::write_claude_profile_launcher(&account.id, config_dir.as_deref(), None, None)?;` (real values in Task 6).
- `crates/usage-daemon/src/daemon.rs:1302-1316` tests: same two-`None` extension as the launchers.rs test.

- [ ] **Step 4: Run**

Run: `cargo test -p usage-daemon launcher 2>&1 | tail -5`
Expected: PASS (all launcher tests, including the pre-existing quoting test).

- [ ] **Step 5: Commit**

```bash
git add crates/usage-daemon/src/providers/launchers.rs crates/usage-daemon/src/providers/claude/adapter.rs crates/usage-daemon/src/daemon.rs
git commit -m "Add working directory and launch flags to the Claude launcher

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Pure launch-plan resolution (merge + persistence rules)

**Files:**
- Modify: `crates/usage-daemon/src/runtime/provider_adapter.rs` (`LaunchOverrides` type)
- Modify: `crates/usage-daemon/src/providers/claude/adapter.rs` (`resolve_launch_plan` + tests)

- [ ] **Step 1: Write the failing tests** in `crates/usage-daemon/src/providers/claude/adapter.rs` `mod tests`:

```rust
    fn saved_settings(
        working_directory: Option<&str>,
        launch: Option<usage_core::LaunchFlags>,
    ) -> settings::ClaudeProfileSettings {
        settings::ClaudeProfileSettings {
            working_directory: working_directory.map(PathBuf::from),
            launch,
            ..Default::default()
        }
    }

    #[test]
    fn launch_plan_uses_saved_prefs_when_no_overrides_arrive() {
        let saved = saved_settings(
            Some("/tmp/saved"),
            Some(usage_core::LaunchFlags {
                model: Some("fable".to_string()),
                ..Default::default()
            }),
        );
        let plan = resolve_launch_plan(&saved, &LaunchOverrides::default()).unwrap();
        assert_eq!(plan.working_directory, Some(PathBuf::from("/tmp/saved")));
        assert_eq!(plan.flags.as_ref().unwrap().model.as_deref(), Some("fable"));
        // A plain open never rewrites config.
        assert!(plan.persist.is_none());
    }

    #[test]
    fn launch_plan_overrides_replace_whole_flag_object_and_persist() {
        let saved = saved_settings(
            Some("/tmp/saved"),
            Some(usage_core::LaunchFlags {
                model: Some("old-model".to_string()),
                effort: Some(usage_core::LaunchEffort::Low),
                dangerously_skip_permissions: false,
            }),
        );
        let overrides = LaunchOverrides {
            working_directory: Some("/tmp/override".to_string()),
            launch: Some(usage_core::LaunchFlags {
                model: None,
                effort: Some(usage_core::LaunchEffort::Max),
                dangerously_skip_permissions: true,
            }),
            remember_dangerously_skip_permissions: false,
        };
        let plan = resolve_launch_plan(&saved, &overrides).unwrap();
        // This run uses the override verbatim (dangerous flag on, once).
        assert_eq!(plan.working_directory, Some(PathBuf::from("/tmp/override")));
        let flags = plan.flags.as_ref().unwrap();
        assert_eq!(flags.model, None);
        assert!(flags.dangerously_skip_permissions);
        // Persisted prefs keep model/effort/cwd but reset the dangerous flag
        // to its saved value because remember was not requested.
        let persist = plan.persist.unwrap();
        assert_eq!(persist.working_directory, Some(PathBuf::from("/tmp/override")));
        let persisted_flags = persist.launch.unwrap();
        assert_eq!(persisted_flags.effort, Some(usage_core::LaunchEffort::Max));
        assert!(!persisted_flags.dangerously_skip_permissions);
    }

    #[test]
    fn launch_plan_remembers_the_dangerous_flag_only_on_request() {
        let saved = saved_settings(None, None);
        let overrides = LaunchOverrides {
            working_directory: None,
            launch: Some(usage_core::LaunchFlags {
                dangerously_skip_permissions: true,
                ..Default::default()
            }),
            remember_dangerously_skip_permissions: true,
        };
        let plan = resolve_launch_plan(&saved, &overrides).unwrap();
        assert!(plan.persist.unwrap().launch.unwrap().dangerously_skip_permissions);
    }

    #[test]
    fn launch_plan_clears_prefs_and_rejects_invalid_models() {
        let saved = saved_settings(Some("/tmp/saved"), None);
        // An empty working directory from the sheet clears the saved value.
        let cleared = resolve_launch_plan(
            &saved,
            &LaunchOverrides {
                working_directory: Some("   ".to_string()),
                launch: Some(usage_core::LaunchFlags::default()),
                remember_dangerously_skip_permissions: false,
            },
        )
        .unwrap();
        assert_eq!(cleared.working_directory, None);
        let persist = cleared.persist.unwrap();
        assert_eq!(persist.working_directory, None);
        // All-default flags normalize to "no flags saved".
        assert_eq!(persist.launch, None);

        let invalid = resolve_launch_plan(
            &saved,
            &LaunchOverrides {
                working_directory: None,
                launch: Some(usage_core::LaunchFlags {
                    model: Some("bad model; rm".to_string()),
                    ..Default::default()
                }),
                remember_dangerously_skip_permissions: false,
            },
        );
        assert!(invalid.is_err());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p usage-daemon launch_plan 2>&1 | tail -5`
Expected: **compile error** — `LaunchOverrides` / `resolve_launch_plan` don't exist.

- [ ] **Step 3: Implement.** In `crates/usage-daemon/src/runtime/provider_adapter.rs`, above the `LaunchHandler` trait (line ~266), add:

```rust
/// Per-open overrides from the confirm-on-open sheet. `None` fields fall back
/// to the account's saved preferences; the sheet always sends the full
/// `launch` object, so an override replaces the whole flag set.
#[derive(Clone, Debug, Default)]
pub(crate) struct LaunchOverrides {
    pub(crate) working_directory: Option<String>,
    pub(crate) launch: Option<usage_core::LaunchFlags>,
    pub(crate) remember_dangerously_skip_permissions: bool,
}
```

In `crates/usage-daemon/src/providers/claude/adapter.rs`, add above the `LaunchHandler` impl (import `LaunchOverrides` in the existing `runtime::provider_adapter` use block):

```rust
pub(crate) struct LaunchPlan {
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) flags: Option<usage_core::LaunchFlags>,
    pub(crate) persist: Option<PersistedLaunchPrefs>,
}

pub(crate) struct PersistedLaunchPrefs {
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) launch: Option<usage_core::LaunchFlags>,
}

/// Merges saved prefs with per-open overrides. Working directory + model +
/// effort persist whenever the sheet sent anything; the dangerous flag
/// persists only when explicitly remembered.
pub(crate) fn resolve_launch_plan(
    saved: &settings::ClaudeProfileSettings,
    overrides: &LaunchOverrides,
) -> anyhow::Result<LaunchPlan> {
    let working_directory = match &overrides.working_directory {
        Some(value) if value.trim().is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => saved.working_directory.clone(),
    };
    let flags = overrides.launch.clone().or_else(|| saved.launch.clone());
    if let Some(flags) = &flags {
        settings::validate_launch_flags(flags)?;
    }
    let persist = (overrides.working_directory.is_some() || overrides.launch.is_some()).then(|| {
        let mut launch = flags.clone();
        if !overrides.remember_dangerously_skip_permissions {
            if let Some(launch) = launch.as_mut() {
                launch.dangerously_skip_permissions = saved
                    .launch
                    .as_ref()
                    .map(|saved| saved.dangerously_skip_permissions)
                    .unwrap_or(false);
            }
        }
        PersistedLaunchPrefs {
            working_directory: working_directory.clone(),
            launch: launch.filter(|flags| flags != &usage_core::LaunchFlags::default()),
        }
    });
    Ok(LaunchPlan {
        working_directory,
        flags,
        persist,
    })
}
```

(`ClaudeProfileSettings` needs its fields constructible from adapter tests — they are `pub(crate)`, same crate, already fine. Add `Default` usage as shown; the struct already derives `Default`.)

- [ ] **Step 4: Run**

Run: `cargo test -p usage-daemon launch_plan 2>&1 | tail -5`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/usage-daemon/src/runtime/provider_adapter.rs crates/usage-daemon/src/providers/claude/adapter.rs
git commit -m "Resolve Claude launch plans from saved prefs and overrides

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Thread overrides through trait → daemon → server; typed errors; capability on

**Files:**
- Modify: `crates/usage-daemon/src/runtime/provider_adapter.rs` (trait + error + capability)
- Modify: `crates/usage-daemon/src/providers/claude/adapter.rs` (launch rewrite)
- Modify: `crates/usage-daemon/src/daemon.rs:556-581` + capability test
- Modify: `crates/usage-daemon/src/server.rs:411-423` + mapping test

- [ ] **Step 1: Write the failing tests.**

In `crates/usage-daemon/src/daemon.rs` `mod tests` (next to `launch_capabilities_match_the_daemon_handlers`, daemon.rs:1318-1327):

```rust
    #[test]
    fn launch_options_capability_is_claude_only() {
        let with_options = provider_registry::descriptors()
            .into_iter()
            .filter(|provider| provider.capabilities.launch_options)
            .map(|provider| provider.id)
            .collect::<Vec<_>>();

        assert_eq!(with_options, vec![ProviderId::new(CLAUDE_PROVIDER_ID)]);
    }
```

In `crates/usage-daemon/src/server.rs` `mod tests`:

```rust
    #[test]
    fn launch_failures_map_invalid_requests_to_invalid_argument() {
        let invalid = launch_failure_response(anyhow::Error::new(
            crate::runtime::provider_adapter::InvalidLaunchRequest(
                "working directory /nope does not exist".to_string(),
            ),
        ));
        let ApiResponse::Error { error } = invalid else {
            panic!("expected error response")
        };
        assert_eq!(error.code, ApiErrorCode::InvalidArgument);

        let other = launch_failure_response(anyhow::anyhow!("Claude is not configured"));
        let ApiResponse::Error { error } = other else {
            panic!("expected error response")
        };
        assert_eq!(error.code, ApiErrorCode::UnsupportedOperation);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p usage-daemon launch_options_capability launch_failures_map 2>&1 | tail -5`
Expected: **compile error** — `InvalidLaunchRequest` / `launch_failure_response` missing; capability filter finds field but derivation still `false` (test would fail red after compile fix).

- [ ] **Step 3: Extend the trait and error type.** In `crates/usage-daemon/src/runtime/provider_adapter.rs` replace the `LaunchHandler` trait (lines 266-273) with:

```rust
/// A launch request the daemon understood but must reject as caller error
/// (bad working directory, malformed flags). The server maps this to
/// `invalid_argument`; everything else stays `unsupported_operation`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct InvalidLaunchRequest(pub(crate) String);

#[async_trait]
pub(crate) trait LaunchHandler: Send + Sync {
    async fn launch(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
        overrides: LaunchOverrides,
    ) -> anyhow::Result<ProviderActionResponse>;

    /// Advertised as the `launch_options` capability: this handler honors
    /// LaunchOverrides and serves per-account launch settings.
    fn supports_launch_options(&self) -> bool {
        false
    }

    async fn launch_settings(
        &self,
        _runtime: ProviderRuntime<'_>,
        account: Account,
    ) -> anyhow::Result<usage_core::AccountLaunchSettingsResponse> {
        anyhow::bail!(
            "launch settings are not supported for {}",
            account.provider_id
        )
    }
}
```

In `descriptor()` (provider_adapter.rs:398-407) replace the staged line from Task 1:

```rust
                launch_options: self
                    .launch_handler()
                    .is_some_and(|handler| handler.supports_launch_options()),
```

- [ ] **Step 4: Rewrite the Claude launch.** In `crates/usage-daemon/src/providers/claude/adapter.rs`, replace the `LaunchHandler` impl (adapter.rs:264-311) with (extend the `runtime::provider_adapter` import list with `InvalidLaunchRequest, LaunchOverrides`):

```rust
#[async_trait]
impl LaunchHandler for ClaudeAdapter {
    async fn launch(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
        overrides: LaunchOverrides,
    ) -> anyhow::Result<ProviderActionResponse> {
        if !account.collection_enabled {
            anyhow::bail!("enable Claude account tracking before opening a profile session");
        }
        let profile_id = account
            .profile_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude account is missing its profile identity"))?;
        let config = runtime.config().await;
        let provider = config
            .providers
            .get(PROVIDER_ID)
            .ok_or_else(|| anyhow::anyhow!("Claude is not configured"))?;
        let (config_dir, saved, has_profile_entry) =
            if provider.profiles.is_empty() && profile_id == "default" {
                (None, settings::ClaudeProfileSettings::default(), false)
            } else {
                let profile = provider
                    .profiles
                    .iter()
                    .find(|profile| {
                        profile.enabled
                            && !profile.deleted
                            && profile.id.as_deref() == Some(profile_id)
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("Claude profile {profile_id} is no longer configured")
                    })?;
                let saved = settings::profile(profile)?;
                let config_dir = saved.claude_config_dir.clone().map(|dir| expand_home_path(&dir));
                (config_dir, saved, true)
            };

        let plan = resolve_launch_plan(&saved, &overrides)
            .map_err(|err| anyhow::Error::new(InvalidLaunchRequest(err.to_string())))?;
        let working_directory = match &plan.working_directory {
            Some(dir) => {
                let expanded = expand_home_path(dir);
                if !expanded.is_dir() {
                    return Err(InvalidLaunchRequest(format!(
                        "working directory {} does not exist",
                        expanded.display()
                    ))
                    .into());
                }
                Some(expanded)
            }
            None => None,
        };

        let launcher = launchers::write_claude_profile_launcher(
            &account.id,
            config_dir.as_deref(),
            working_directory.as_deref(),
            plan.flags.as_ref(),
        )?;
        launchers::open_terminal(&launcher)?;

        if let (Some(persist), true) = (plan.persist, has_profile_entry) {
            runtime
                .mutate_config(|config| {
                    let provider =
                        config.providers.entry(PROVIDER_ID.to_string()).or_default();
                    if let Some(profile) = provider.profiles.iter_mut().find(|profile| {
                        profile.enabled
                            && !profile.deleted
                            && profile.id.as_deref() == Some(profile_id)
                    }) {
                        settings::update_profile(profile, |settings| {
                            settings.working_directory = persist.working_directory.clone();
                            settings.launch = persist.launch.clone();
                        })?;
                    }
                    Ok(())
                })
                .await?;
        }

        Ok(ProviderActionResponse {
            provider_id: account.provider_id,
            message: format!(
                "Opened a Claude session for {}. Activity from this terminal stays with this profile.",
                account.display_name.as_deref().unwrap_or(profile_id)
            ),
            authentication_url: None,
        })
    }

    fn supports_launch_options(&self) -> bool {
        true
    }
}
```

- [ ] **Step 5: Plumb daemon + server.** In `crates/usage-daemon/src/daemon.rs:556-581`, change the signature and handler call:

```rust
    pub async fn launch_provider_account(
        &self,
        account_id: AccountId,
        overrides: crate::runtime::provider_adapter::LaunchOverrides,
    ) -> anyhow::Result<ProviderActionResponse> {
        if self.fixture_mode {
            anyhow::bail!("provider launch is unavailable in development fixture mode");
        }
        let account = self
            .storage
            .account(&account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
        let adapter = provider_registry::adapter(&account.provider_id)?;
        let handler = adapter.launch_handler().ok_or_else(|| {
            anyhow::anyhow!(
                "profile sessions are not supported for {}",
                account.provider_id
            )
        })?;
        if !handler.supports_launch_options()
            && (overrides.working_directory.is_some() || overrides.launch.is_some())
        {
            anyhow::bail!(
                "launch overrides are not supported for {}",
                account.provider_id
            );
        }
        handler
            .launch(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                account,
                overrides,
            )
            .await
    }
```

In `crates/usage-daemon/src/server.rs` replace the Task 1 arm head and body (server.rs:411-423) with:

```rust
            ApiRequest::LaunchProviderAccount {
                account_id,
                working_directory,
                launch,
                remember_dangerously_skip_permissions,
            } => {
                if let Some(error) = self.account_validation_error(&account_id).await {
                    error
                } else {
                    let overrides = crate::runtime::provider_adapter::LaunchOverrides {
                        working_directory,
                        launch,
                        remember_dangerously_skip_permissions,
                    };
                    match self
                        .runtime
                        .launch_provider_account(account_id, overrides)
                        .await
                    {
                        Ok(action) => ApiResponse::ProviderAction { action },
                        Err(err) => {
                            warn!(error = %err, "provider account launch failed");
                            launch_failure_response(err)
                        }
                    }
                }
            }
```

And add near the other free helper functions in server.rs (e.g. next to `storage_error`):

```rust
fn launch_failure_response(err: anyhow::Error) -> ApiResponse {
    if err
        .downcast_ref::<crate::runtime::provider_adapter::InvalidLaunchRequest>()
        .is_some()
    {
        ApiResponse::error(ApiErrorCode::InvalidArgument, err.to_string())
    } else {
        ApiResponse::error(ApiErrorCode::UnsupportedOperation, err.to_string())
    }
}
```

- [ ] **Step 6: Run the full daemon suite**

Run: `cargo test -p usage-daemon 2>&1 | tail -5`
Expected: PASS — including `launch_options_capability_is_claude_only`, `launch_failures_map_invalid_requests_to_invalid_argument`, and the untouched `launch_capabilities_match_the_daemon_handlers`.

- [ ] **Step 7: Commit**

```bash
git add crates/usage-daemon/src/runtime/provider_adapter.rs crates/usage-daemon/src/providers/claude/adapter.rs crates/usage-daemon/src/daemon.rs crates/usage-daemon/src/server.rs
git commit -m "Thread launch overrides through the launch pipeline

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: `get_account_launch_settings` read path

**Files:**
- Modify: `crates/usage-daemon/src/providers/claude/adapter.rs` (pure response fn + trait impl + tests)
- Modify: `crates/usage-daemon/src/daemon.rs` (new runtime method)
- Modify: `crates/usage-daemon/src/server.rs` (replace Task 1 stub arm + socket test)

- [ ] **Step 1: Write the failing pure-function test** in `crates/usage-daemon/src/providers/claude/adapter.rs` `mod tests`:

```rust
    #[test]
    fn launch_settings_response_reports_managed_dir_and_prefs() {
        let now = chrono::Utc::now();
        let account = Account {
            id: usage_core::AccountId::new("account-1"),
            provider_id: ProviderId::new(PROVIDER_ID),
            external_account_id: "user@example.com".to_string(),
            profile_id: Some("work".to_string()),
            display_name: None,
            display_name_source: usage_core::AccountDisplayNameSource::Generated,
            email: None,
            hidden: false,
            collection_enabled: true,
            created_at: now,
            updated_at: now,
        };
        let managed_dir =
            usage_core::default_app_dir().map(|root| root.join("profiles/claude/work"));
        let settings = settings::ClaudeProfileSettings {
            claude_config_dir: managed_dir,
            working_directory: Some(PathBuf::from("/tmp/work")),
            launch: Some(usage_core::LaunchFlags {
                model: Some("fable".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let response = account_launch_settings_response(&account, &settings);
        assert_eq!(response.account_id.as_str(), "account-1");
        assert_eq!(response.working_directory.as_deref(), Some("/tmp/work"));
        assert_eq!(response.launch.unwrap().model.as_deref(), Some("fable"));
        assert!(response.has_managed_config_dir);

        let unmanaged = account_launch_settings_response(
            &account,
            &settings::ClaudeProfileSettings::default(),
        );
        assert!(!unmanaged.has_managed_config_dir);
        assert_eq!(unmanaged.working_directory, None);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p usage-daemon launch_settings_response_reports 2>&1 | tail -5`
Expected: **compile error** — `account_launch_settings_response` doesn't exist.

- [ ] **Step 3: Implement.** In `crates/usage-daemon/src/providers/claude/adapter.rs` add (near `resolve_launch_plan`; import `crate::runtime::managed_profiles` and `usage_core::AccountLaunchSettingsResponse`):

```rust
pub(crate) fn account_launch_settings_response(
    account: &Account,
    settings: &settings::ClaudeProfileSettings,
) -> AccountLaunchSettingsResponse {
    let has_managed_config_dir = settings.claude_config_dir.as_ref().is_some_and(|dir| {
        managed_profiles::is_managed_profile(&expand_home_path(dir), PROVIDER_ID)
    });
    AccountLaunchSettingsResponse {
        provider_id: account.provider_id.clone(),
        account_id: account.id.clone(),
        working_directory: settings
            .working_directory
            .as_ref()
            .map(|path| path.display().to_string()),
        launch: settings.launch.clone(),
        has_managed_config_dir,
    }
}
```

Add to the `LaunchHandler` impl for `ClaudeAdapter` (after `supports_launch_options`):

```rust
    async fn launch_settings(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
    ) -> anyhow::Result<AccountLaunchSettingsResponse> {
        let profile_id = account
            .profile_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude account is missing its profile identity"))?;
        let config = runtime.config().await;
        let saved = config
            .providers
            .get(PROVIDER_ID)
            .and_then(|provider| {
                provider.profiles.iter().find(|profile| {
                    profile.enabled
                        && !profile.deleted
                        && profile.id.as_deref() == Some(profile_id)
                })
            })
            .map(settings::profile)
            .transpose()?
            .unwrap_or_default();
        Ok(account_launch_settings_response(&account, &saved))
    }
```

In `crates/usage-daemon/src/daemon.rs` add after `launch_provider_account`:

```rust
    /// Read-only, so fixture mode is allowed — the Open sheet stays demoable
    /// via `just fixture` even though the launch itself is rejected there.
    pub async fn account_launch_settings(
        &self,
        account_id: AccountId,
    ) -> anyhow::Result<usage_core::AccountLaunchSettingsResponse> {
        let account = self
            .storage
            .account(&account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
        let adapter = provider_registry::adapter(&account.provider_id)?;
        let handler = adapter.launch_handler().ok_or_else(|| {
            anyhow::anyhow!(
                "launch settings are not supported for {}",
                account.provider_id
            )
        })?;
        anyhow::ensure!(
            handler.supports_launch_options(),
            "launch settings are not supported for {}",
            account.provider_id
        );
        handler
            .launch_settings(
                crate::runtime::provider_adapter::ProviderRuntime::new(self),
                account,
            )
            .await
    }
```

Replace the Task 1 stub arm in `crates/usage-daemon/src/server.rs`:

```rust
            ApiRequest::GetAccountLaunchSettings { account_id } => {
                if let Some(error) = self.account_validation_error(&account_id).await {
                    error
                } else {
                    match self.runtime.account_launch_settings(account_id).await {
                        Ok(settings) => ApiResponse::AccountLaunchSettings { settings },
                        Err(err) => {
                            warn!(error = %err, "account launch settings lookup failed");
                            ApiResponse::error(ApiErrorCode::UnsupportedOperation, err.to_string())
                        }
                    }
                }
            }
```

- [ ] **Step 4: Add the routing test** in `crates/usage-daemon/src/server.rs` `mod tests` (mirrors `serves_fixture_accounts_usage_and_notifications_over_socket` at server.rs:1120 — same `test_env` + `fixtures::seed` helpers, but via `handle_request` directly):

```rust
    #[tokio::test]
    async fn account_launch_settings_route_by_provider_capability() {
        let env = test_env(BTreeMap::new());
        crate::fixtures::seed(
            &env.runtime.storage,
            crate::fixtures::FixtureScenario::Notifications,
        )
        .await
        .unwrap();
        let server = SocketServer::new(env.runtime.clone());

        let unknown = server
            .handle_request(ApiRequest::GetAccountLaunchSettings {
                account_id: usage_core::AccountId::new("definitely-unknown"),
            })
            .await;
        let ApiResponse::Error { error } = unknown else {
            panic!("unexpected response: {unknown:?}")
        };
        assert_eq!(error.code, ApiErrorCode::UnknownAccount);

        let ApiResponse::Accounts { accounts } =
            server.handle_request(ApiRequest::GetAccounts).await
        else {
            panic!("expected accounts")
        };
        let codex = accounts
            .iter()
            .find(|account| account.provider_id.as_str() == "codex")
            .expect("fixture codex account");
        let unsupported = server
            .handle_request(ApiRequest::GetAccountLaunchSettings {
                account_id: codex.id.clone(),
            })
            .await;
        let ApiResponse::Error { error } = unsupported else {
            panic!("unexpected response: {unsupported:?}")
        };
        assert_eq!(error.code, ApiErrorCode::UnsupportedOperation);

        let claude = accounts
            .iter()
            .find(|account| account.provider_id.as_str() == "claude")
            .expect("fixture claude account");
        let supported = server
            .handle_request(ApiRequest::GetAccountLaunchSettings {
                account_id: claude.id.clone(),
            })
            .await;
        let ApiResponse::AccountLaunchSettings { settings } = supported else {
            panic!("unexpected response: {supported:?}")
        };
        assert_eq!(settings.provider_id.as_str(), "claude");
        assert_eq!(settings.working_directory, None);
        assert!(!settings.has_managed_config_dir);

        let _ = std::fs::remove_dir_all(env.root);
    }
```

- [ ] **Step 5: Run**

Run: `cargo test -p usage-daemon account_launch_settings launch_settings_response 2>&1 | tail -5`
Expected: PASS. Then `cargo test --workspace 2>&1 | tail -3` — PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/usage-daemon/src/providers/claude/adapter.rs crates/usage-daemon/src/daemon.rs crates/usage-daemon/src/server.rs
git commit -m "Expose per-account launch settings over the socket

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: API + product docs

**Files:**
- Modify: `docs/api/methods.md`, `docs/api/versioning.md`, `docs/claude.md`, `CHANGELOG.md`

- [ ] **Step 1: `docs/api/methods.md`.** Add to the requests/responses table (after the `launch_provider_account` row, methods.md:33):

```markdown
| `get_account_launch_settings` | `{"method":"get_account_launch_settings","account_id":"ACCOUNT"}` | `account_launch_settings` |
```

Add to the read-methods table (methods.md:44-57):

```markdown
| `get_account_launch_settings` | The account's saved launch preferences: working directory, structured launch flags, and whether the profile has a managed config directory. Providers must advertise `launch_options`. | `unknown_account`, `unsupported_operation` | 3s |
```

In the external-action table, replace the `launch_provider_account` effect cell with:

```markdown
| `launch_provider_account` | Opens the provider with the account's isolated profile. Optional `working_directory`, `launch` (structured flags), and `remember_dangerously_skip_permissions` override and — on success — persist the account's saved preferences (the dangerous flag persists only when explicitly remembered). Providers must advertise `launch_options` for overrides; a missing or nonexistent working directory fails with `invalid_argument`. | Not idempotent — it may open several sessions. No job persists. | `unknown_account`, `storage_unavailable`, `unsupported_operation`, `invalid_argument` |
```

- [ ] **Step 2: `docs/api/versioning.md`.** In the provider-capabilities sentence (versioning.md:22), extend the list: `multiple_accounts`, `add_account`, `repair`, `launch_account`, `launch_options`, and generic `setup`.

- [ ] **Step 3: `docs/claude.md`.** In the security-notes section (the paragraph about per-profile launch), append:

```markdown
Managed accounts can save a per-account working directory and structured launch flags (model, effort, dangerously-skip-permissions). The generated launcher refuses to run if the working directory no longer exists, and the dangerous flag is use-once unless you explicitly remember it.
```

- [ ] **Step 4: `CHANGELOG.md`** under `## Unreleased`:

```markdown
### App

- Added a confirm-on-open sheet for Claude sessions: per-account working directory and structured launch flags (model, effort, dangerously-skip-permissions), persisted on successful open. The dangerous flag is use-once unless explicitly remembered.
```

- [ ] **Step 5: Check for stale references**

Run: `grep -rn "launch_provider_account" docs/ | grep -v schemas`
Expected: only the rows you just edited (plus `docs/api/index.md` if it enumerates methods — update its count/list if so).

- [ ] **Step 6: Commit**

```bash
git add docs/api/methods.md docs/api/versioning.md docs/claude.md CHANGELOG.md
git commit -m "Document launch preferences API

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: Swift wire layer

**Files:**
- Create: `apps/UsageMenuBar/Sources/UsageMenuBar/Networking/Models/AccountLaunchSettings.swift`
- Modify: `apps/UsageMenuBar/Sources/UsageMenuBar/Networking/Wire.swift`
- Modify: `apps/UsageMenuBar/Sources/UsageMenuBar/Networking/DaemonClient.swift`
- Modify: `apps/UsageMenuBar/Sources/UsageMenuBar/App/AppState.swift:437-438` (call-site compatibility only)
- Test: `apps/UsageMenuBar/Tests/UsageMenuBarTests/ReliabilityTests.swift`

- [ ] **Step 1: Write the failing tests** in `ReliabilityTests.swift` inside `DaemonClientTests` (class starts at line 280):

```swift
    func testEncodesLaunchOverridesAndOmitsThemWhenAbsent() throws {
        let full = DaemonRequest.launchProviderAccount(
            accountId: "account-1",
            workingDirectory: "/tmp/demo",
            launch: LaunchFlags(model: "fable", effort: "xhigh", dangerouslySkipPermissions: true),
            rememberDangerouslySkipPermissions: true
        )
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder.usage.encode(full)) as? [String: Any]
        )
        XCTAssertEqual(object["method"] as? String, "launch_provider_account")
        XCTAssertEqual(object["working_directory"] as? String, "/tmp/demo")
        XCTAssertEqual(object["remember_dangerously_skip_permissions"] as? Bool, true)
        let launch = try XCTUnwrap(object["launch"] as? [String: Any])
        XCTAssertEqual(launch["model"] as? String, "fable")
        XCTAssertEqual(launch["effort"] as? String, "xhigh")
        XCTAssertEqual(launch["dangerously_skip_permissions"] as? Bool, true)

        let bare = DaemonRequest.launchProviderAccount(
            accountId: "account-1",
            workingDirectory: nil,
            launch: nil,
            rememberDangerouslySkipPermissions: false
        )
        let bareObject = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder.usage.encode(bare)) as? [String: Any]
        )
        XCTAssertNil(bareObject["working_directory"])
        XCTAssertNil(bareObject["launch"])
        XCTAssertNil(bareObject["remember_dangerously_skip_permissions"])
    }

    func testDecodesAccountLaunchSettings() throws {
        let response = try JSONDecoder.usage.decode(
            DaemonResponse.self,
            from: Data(#"{"api_version":3,"type":"account_launch_settings","settings":{"provider_id":"claude","account_id":"account-1","working_directory":"~/Projects/demo","launch":{"model":"fable","effort":"xhigh","dangerously_skip_permissions":true},"has_managed_config_dir":true}}"#.utf8)
        )
        guard case let .accountLaunchSettings(settings) = response else {
            return XCTFail("expected account launch settings")
        }
        XCTAssertEqual(settings.workingDirectory, "~/Projects/demo")
        XCTAssertEqual(settings.launch?.model, "fable")
        XCTAssertEqual(settings.launch?.effort, "xhigh")
        XCTAssertEqual(settings.launch?.dangerouslySkipPermissions, true)
        XCTAssertTrue(settings.hasManagedConfigDir)
    }
```

Also extend `testProviderCapabilitiesRemainIndependent` (ReliabilityTests.swift:296-316): add `launchOptions: true` to the `ProviderCapabilities(...)` construction and add:

```swift
        XCTAssertTrue(providerSupports("fixture", capability: \.launchOptions, in: providers))
```

- [ ] **Step 2: Run to verify failure**

Run: `swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete --filter DaemonClientTests 2>&1 | tail -5`
Expected: **compile error** — `LaunchFlags` / case signature mismatch.

- [ ] **Step 3: Create `apps/UsageMenuBar/Sources/UsageMenuBar/Networking/Models/AccountLaunchSettings.swift`:**

```swift
import Foundation

/// Structured launch flags mirrored from the daemon's `LaunchFlags`.
/// Decoding rides the snake_case-converting decoder; encoding writes explicit
/// snake_case keys because `JSONEncoder.usage` has no key strategy.
struct LaunchFlags: Equatable, Sendable, Codable {
    var model: String?
    var effort: String?
    var dangerouslySkipPermissions: Bool

    init(model: String? = nil, effort: String? = nil, dangerouslySkipPermissions: Bool = false) {
        self.model = model
        self.effort = effort
        self.dangerouslySkipPermissions = dangerouslySkipPermissions
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: DecodeKeys.self)
        model = try c.decodeIfPresent(String.self, forKey: .model)
        effort = try c.decodeIfPresent(String.self, forKey: .effort)
        dangerouslySkipPermissions =
            try c.decodeIfPresent(Bool.self, forKey: .dangerouslySkipPermissions) ?? false
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: EncodeKeys.self)
        try c.encodeIfPresent(model, forKey: .model)
        try c.encodeIfPresent(effort, forKey: .effort)
        if dangerouslySkipPermissions {
            try c.encode(true, forKey: .dangerouslySkipPermissions)
        }
    }

    private enum DecodeKeys: String, CodingKey { case model, effort, dangerouslySkipPermissions }
    private enum EncodeKeys: String, CodingKey {
        case model, effort
        case dangerouslySkipPermissions = "dangerously_skip_permissions"
    }
}

struct AccountLaunchSettingsResponse: Decodable, Equatable, Sendable {
    let providerId: String
    let accountId: String
    var workingDirectory: String?
    var launch: LaunchFlags?
    let hasManagedConfigDir: Bool
}
```

- [ ] **Step 4: Extend `Wire.swift`.**

Request case (Wire.swift:19): replace `case launchProviderAccount(accountId: String)` with:

```swift
    case launchProviderAccount(
        accountId: String,
        workingDirectory: String?,
        launch: LaunchFlags?,
        rememberDangerouslySkipPermissions: Bool
    )
    case getAccountLaunchSettings(accountId: String)
```

Encoding (Wire.swift:70-72): replace the `.launchProviderAccount` case with:

```swift
        case .launchProviderAccount(let accountId, let workingDirectory, let launch, let remember):
            try c.encode("launch_provider_account", forKey: .method)
            try c.encode(accountId, forKey: .accountId)
            try c.encodeIfPresent(workingDirectory, forKey: .workingDirectory)
            try c.encodeIfPresent(launch, forKey: .launch)
            if remember { try c.encode(true, forKey: .rememberDangerouslySkipPermissions) }
        case .getAccountLaunchSettings(let accountId):
            try c.encode("get_account_launch_settings", forKey: .method)
            try c.encode(accountId, forKey: .accountId)
```

CodingKeys `K` (Wire.swift:75-85): add `case launch` to the bare list and:

```swift
        case workingDirectory = "working_directory"
        case rememberDangerouslySkipPermissions = "remember_dangerously_skip_permissions"
```

`ProviderCapabilities` (Wire.swift:157-194): add `let launchOptions: Bool`; in the labeled init add parameter `launchOptions: Bool = false` (after `launchAccount:`) and `self.launchOptions = launchOptions`; in `init(from:)` add `launchOptions = try c.decodeIfPresent(Bool.self, forKey: .launchOptions) ?? false`; add `launchOptions` to `CodingKeys`.

`DaemonResponse` (Wire.swift:204-248): add case `accountLaunchSettings(AccountLaunchSettingsResponse)`; in the switch add:

```swift
        case "account_launch_settings":
            self = .accountLaunchSettings(
                try c.decode(AccountLaunchSettingsResponse.self, forKey: .settings)
            )
```

and add `settings` to `DaemonResponse.K`.

- [ ] **Step 5: Extend `DaemonClient.swift`.** Replace `launchProviderAccount` (DaemonClient.swift:75-78) with:

```swift
    func launchProviderAccount(
        accountId: String,
        workingDirectory: String? = nil,
        launch: LaunchFlags? = nil,
        rememberDangerouslySkipPermissions: Bool = false
    ) async throws -> ProviderActionResponse {
        guard case let .providerAction(v) = try await send(.launchProviderAccount(
            accountId: accountId,
            workingDirectory: workingDirectory,
            launch: launch,
            rememberDangerouslySkipPermissions: rememberDangerouslySkipPermissions
        )) else { throw DaemonError.badResponse }
        return v
    }

    func accountLaunchSettings(accountId: String) async throws -> AccountLaunchSettingsResponse {
        guard case let .accountLaunchSettings(v) = try await send(.getAccountLaunchSettings(accountId: accountId)) else { throw DaemonError.badResponse }
        return v
    }
```

In `DaemonRequestTimeout` (DaemonClient.swift:114-133) add `.getAccountLaunchSettings` to the 3-second read group.

- [ ] **Step 6: Build + test**

Run: `swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete 2>&1 | tail -5`
Expected: PASS (existing `AppState.swift:438` call site compiles unchanged thanks to the defaulted client parameters).

- [ ] **Step 7: Commit**

```bash
git add apps/UsageMenuBar/Sources/UsageMenuBar/Networking apps/UsageMenuBar/Tests/UsageMenuBarTests/ReliabilityTests.swift
git commit -m "Add launch settings to the Swift wire layer

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: Open-session model + AppState glue

**Files:**
- Create: `apps/UsageMenuBar/Sources/UsageMenuBar/State/OpenSession.swift`
- Modify: `apps/UsageMenuBar/Sources/UsageMenuBar/App/AppState.swift`
- Create: `apps/UsageMenuBar/Tests/UsageMenuBarTests/OpenSessionModelTests.swift`

- [ ] **Step 1: Write the failing tests** — create `OpenSessionModelTests.swift`:

```swift
import XCTest
@testable import UsageMenuBar

final class OpenSessionModelTests: XCTestCase {
    private func settings(
        workingDirectory: String? = nil,
        launch: LaunchFlags? = nil,
        hasManagedConfigDir: Bool = true
    ) -> AccountLaunchSettingsResponse {
        AccountLaunchSettingsResponse(
            providerId: "claude",
            accountId: "account-1",
            workingDirectory: workingDirectory,
            launch: launch,
            hasManagedConfigDir: hasManagedConfigDir
        )
    }

    func testPrefillsFromSavedSettingsAndDefaultsRememberOff() {
        let model = OpenSessionModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            settings: settings(
                workingDirectory: "~/Projects/demo",
                launch: LaunchFlags(model: "fable", effort: "xhigh", dangerouslySkipPermissions: true)
            )
        )
        XCTAssertEqual(model.workingDirectory, "~/Projects/demo")
        XCTAssertEqual(model.model, "fable")
        XCTAssertEqual(model.effort, .xhigh)
        XCTAssertTrue(model.dangerouslySkipPermissions)
        // The dangerous flag is use-once by default.
        XCTAssertFalse(model.rememberDangerous)
    }

    func testUnknownEffortAndMissingPrefsFallBackToDefaults() {
        let model = OpenSessionModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            settings: settings(launch: LaunchFlags(effort: "warp-speed"))
        )
        XCTAssertEqual(model.effort, .systemDefault)
        XCTAssertEqual(model.workingDirectory, "")
        XCTAssertEqual(model.model, "")
    }

    func testWireFlagsTrimAndNilOutDefaults() {
        var model = OpenSessionModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            settings: settings()
        )
        model.model = "  fable  "
        model.effort = .systemDefault
        model.workingDirectory = "  /tmp/demo  "

        XCTAssertEqual(model.wireFlags, LaunchFlags(model: "fable"))
        XCTAssertEqual(model.trimmedWorkingDirectory, "/tmp/demo")

        model.model = "   "
        XCTAssertNil(model.wireFlags.model)
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete --filter OpenSessionModelTests 2>&1 | tail -5`
Expected: **compile error** — `OpenSessionModel` doesn't exist.

- [ ] **Step 3: Create `apps/UsageMenuBar/Sources/UsageMenuBar/State/OpenSession.swift`:**

```swift
import Foundation

/// Wire values for `--effort`; `systemDefault` sends nothing. Kept in sync
/// with the daemon's closed `LaunchEffort` enum.
enum EffortChoice: String, CaseIterable, Identifiable, Sendable {
    case systemDefault = "default"
    case low, medium, high, xhigh, max

    var id: String { rawValue }
    var label: String { self == .systemDefault ? "Default" : rawValue }
    var wireValue: String? { self == .systemDefault ? nil : rawValue }
}

/// Editable state behind the confirm-on-open sheet. Pure value type so the
/// prefill/trim/wire-mapping rules are unit-testable without UI.
struct OpenSessionModel: Equatable, Sendable {
    let accountId: String
    let accountTitle: String
    let providerId: String
    let hasManagedConfigDir: Bool
    var workingDirectory: String
    var model: String
    var effort: EffortChoice
    var dangerouslySkipPermissions: Bool
    var rememberDangerous: Bool

    init(
        accountId: String,
        accountTitle: String,
        providerId: String,
        settings: AccountLaunchSettingsResponse
    ) {
        self.accountId = accountId
        self.accountTitle = accountTitle
        self.providerId = providerId
        hasManagedConfigDir = settings.hasManagedConfigDir
        workingDirectory = settings.workingDirectory ?? ""
        model = settings.launch?.model ?? ""
        effort = (settings.launch?.effort).flatMap(EffortChoice.init(rawValue:)) ?? .systemDefault
        dangerouslySkipPermissions = settings.launch?.dangerouslySkipPermissions ?? false
        rememberDangerous = false
    }

    var trimmedWorkingDirectory: String {
        workingDirectory.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// The sheet is authoritative: it always sends a full flag object, so
    /// clearing a field here clears the saved preference on a successful open.
    var wireFlags: LaunchFlags {
        let trimmedModel = model.trimmingCharacters(in: .whitespacesAndNewlines)
        return LaunchFlags(
            model: trimmedModel.isEmpty ? nil : trimmedModel,
            effort: effort.wireValue,
            dangerouslySkipPermissions: dangerouslySkipPermissions
        )
    }
}
```

- [ ] **Step 4: AppState glue.** In `apps/UsageMenuBar/Sources/UsageMenuBar/App/AppState.swift`:

Near the other `@Published` properties (around line 50):

```swift
    @Published var openSession: OpenSessionModel?
```

Next to `supportsLaunchAccount` (AppState.swift:882-884):

```swift
    func supportsLaunchOptions(_ providerId: String) -> Bool {
        providerSupports(providerId, capability: \.launchOptions, in: serverProviders)
    }
```

After `launchProviderAccount` (AppState.swift:428-441):

```swift
    func prepareOpenSession(_ accountId: String) async {
        guard let account = accounts.first(where: { $0.id == accountId }) else {
            actionError = "The selected account is no longer available."
            return
        }
        guard supportsLaunchOptions(account.providerId) else {
            await launchProviderAccount(accountId)
            return
        }
        await perform(.account(accountId)) {
            let settings = try await client.accountLaunchSettings(accountId: accountId)
            let title = (account.displayName?.isEmpty == false ? account.displayName : nil)
                ?? account.externalAccountId
            openSession = OpenSessionModel(
                accountId: account.id,
                accountTitle: title,
                providerId: account.providerId,
                settings: settings
            )
        }
    }

    func confirmOpenSession(_ model: OpenSessionModel) async {
        openSession = model
        await perform(.account(model.accountId)) {
            let response = try await client.launchProviderAccount(
                accountId: model.accountId,
                workingDirectory: model.trimmedWorkingDirectory,
                launch: model.wireFlags,
                rememberDangerouslySkipPermissions: model.rememberDangerous
            )
            actionMessage = response.message
            openSession = nil
        }
    }
```

(On failure `perform` records `actionError` and `openSession` stays non-nil, so the sheet remains open for correction.)

- [ ] **Step 5: Run**

Run: `swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add apps/UsageMenuBar/Sources/UsageMenuBar/State/OpenSession.swift apps/UsageMenuBar/Sources/UsageMenuBar/App/AppState.swift apps/UsageMenuBar/Tests/UsageMenuBarTests/OpenSessionModelTests.swift
git commit -m "Add the open-session sheet model

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 11: Confirm-on-open UI + wiring

**Files:**
- Create: `apps/UsageMenuBar/Sources/UsageMenuBar/Views/Settings/OpenSessionSheet.swift`
- Modify: `apps/UsageMenuBar/Sources/UsageMenuBar/Views/Settings/Settings.swift:435-440`

- [ ] **Step 1: Create the sheet + window presenter** — `OpenSessionSheet.swift`. A dedicated `NSWindow` (not the transient Settings popover) so the `NSOpenPanel` taking key focus cannot dismiss the flow:

```swift
import AppKit
import SwiftUI

/// Hosts the confirm-on-open sheet in its own window: the Settings popover is
/// transient and would close when NSOpenPanel takes key focus.
@MainActor
final class OpenSessionWindow {
    static let shared = OpenSessionWindow()
    private var window: NSWindow?

    func present(state: AppState, model: OpenSessionModel) {
        close()
        let hosting = NSHostingController(
            rootView: OpenSessionSheet(state: state, model: model) { [weak self] in
                self?.close()
            }
        )
        let window = NSWindow(contentViewController: hosting)
        window.title = "Open Claude Session"
        window.styleMask = [.titled, .closable]
        window.level = .floating
        window.isReleasedWhenClosed = false
        window.center()
        self.window = window
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    func close() {
        window?.close()
        window = nil
    }
}

struct OpenSessionSheet: View {
    @ObservedObject var state: AppState
    @State private var model: OpenSessionModel
    let dismiss: () -> Void

    init(state: AppState, model: OpenSessionModel, dismiss: @escaping () -> Void) {
        self.state = state
        _model = State(initialValue: model)
        self.dismiss = dismiss
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Open Claude session for \(model.accountTitle)")
                .font(.headline)

            VStack(alignment: .leading, spacing: 4) {
                Text("Working directory").font(.subheadline)
                HStack {
                    TextField("Optional — launches in home folder", text: $model.workingDirectory)
                        .textFieldStyle(.roundedBorder)
                    Button("Choose…") { chooseFolder() }
                }
            }

            VStack(alignment: .leading, spacing: 4) {
                Text("Launch flags").font(.subheadline)
                TextField("Model (optional, e.g. fable)", text: $model.model)
                    .textFieldStyle(.roundedBorder)
                Picker("Effort", selection: $model.effort) {
                    ForEach(EffortChoice.allCases) { choice in
                        Text(choice.label).tag(choice)
                    }
                }
                Toggle("Skip permission prompts (dangerous)", isOn: $model.dangerouslySkipPermissions)
                if model.dangerouslySkipPermissions {
                    Toggle("Remember for this account", isOn: $model.rememberDangerous)
                        .padding(.leading, 20)
                        .help("Off means the dangerous flag applies to this session only.")
                }
            }

            if let error = state.actionError {
                Text(error).font(.caption).foregroundStyle(.red)
            }

            HStack {
                Spacer()
                Button("Cancel") {
                    state.openSession = nil
                    dismiss()
                }
                Button("Open") {
                    Task {
                        await state.confirmOpenSession(model)
                        if state.openSession == nil { dismiss() }
                    }
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        let current = model.trimmedWorkingDirectory
        if !current.isEmpty {
            panel.directoryURL = URL(
                fileURLWithPath: (current as NSString).expandingTildeInPath
            )
        }
        if panel.runModal() == .OK, let url = panel.url {
            model.workingDirectory = url.path
        }
    }
}
```

- [ ] **Step 2: Wire the account menu.** In `Settings.swift` replace the launch button (Settings.swift:435-440):

```swift
            if state.supportsLaunchAccount(account.providerId), !isRemoved {
                Button("Open \(ProviderCatalog.name(for: account.providerId)) session") {
                    if state.supportsLaunchOptions(account.providerId) {
                        Task { @MainActor in
                            await state.prepareOpenSession(account.id)
                            if let model = state.openSession {
                                OpenSessionWindow.shared.present(state: state, model: model)
                            }
                        }
                    } else {
                        Task { await state.launchProviderAccount(account.id) }
                    }
                }
                Divider()
            }
```

- [ ] **Step 3: Build and test**

Run: `swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete 2>&1 | tail -5`
Expected: PASS (view code compiles under strict concurrency; no new unit tests — view is thin over the tested model).

- [ ] **Step 4: Manual smoke check via fixtures**

Run: `just fixture`
Expected: app launches against fixture data. Claude fixture account → "Open Claude session" opens the confirm window with empty prefs (capability derives from the real adapter registry, so it's on in fixture mode); pressing Open surfaces the fixture-mode launch rejection as an inline error — correct per design (`preview`/read allowed, side effects rejected). Close the app afterward.

- [ ] **Step 5: Commit**

```bash
git add apps/UsageMenuBar/Sources/UsageMenuBar/Views/Settings
git commit -m "Present the Claude open-session confirm window

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 12: Full verification sweep

- [ ] **Step 1: Full repo check**

Run: `just check`
Expected: `fmt-check`, `clippy`, `cargo test --workspace`, Swift build+test (strict concurrency), and `audit` all pass. Fix anything it flags (clippy on new code, fmt) and amend the relevant commit or add a small "Address clippy findings in launch prefs" commit.

- [ ] **Step 2: End-to-end sanity on the real daemon (manual, non-fixture)** — build and run the dev app, open the confirm sheet on a managed Claude account, set a working directory + flags, Open; verify Terminal opens in that directory, then reopen the sheet and confirm the prefs prefilled. Verify a deleted directory produces the `invalid_argument` error inline instead of launching.

- [ ] **Step 3: Review the branch**

Run: `git log --oneline main..HEAD && git diff main --stat`
Expected: ~11 commits matching this plan; no stray files (check `git status` is clean).

---

## Self-Review Notes (already applied)

- **Spec coverage (PR1 list from the design doc):** settings fields + `profile_setting_keys` parity → Task 3; launcher cwd/flags + cd guard → Task 4; `LaunchOptions` → Tasks 5-6; read surface → Task 7; `launch_options` capability → Tasks 1+6; Swift Open sheet → Tasks 9-11; wire artifacts (supports_method, schema regen, fixture, methods.md, Wire.swift) → Tasks 1, 2, 8, 9. Persistence semantics (dangerous flag use-once) → Tasks 5, 10, 11. `invalid_argument` mapping → Task 6. Fixture-mode policy (reject launch, allow read) → Task 7.
- **Deliberate scope cuts (per design):** no importer, no `local_import` settings field (PR2), no capability for import, no project-transcript anything.
- **Known seams for the executor:** `usage-cli` matches `ApiResponse` non-exhaustively (verified) — Task 1 Step 6 covers the fallback if a match is exhaustive somewhere; `docs/api/index.md` may enumerate methods — Task 8 Step 5 greps for it; effort enum values were verified against `claude --help` (2.1.218) — re-verify at execution per the design's rollout note.
