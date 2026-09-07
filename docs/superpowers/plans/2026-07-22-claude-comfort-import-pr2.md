# Claude Comfort-Pack Import (PR2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Manual prefs-only comfort-pack import from local `~/.claude` into a managed Claude profile (scrubbed settings + trust + `history.jsonl`), via a job-shaped socket API and an Import sheet — per `docs/superpowers/specs/2026-07-22-claude-launch-import-design.md` PR2 slice (**no `projects/`, no plugins**).

**Architecture:** Additive v3 wire types (`ImportOptions`, `ImportMode`, `ImportJob`, preview/import/get-job methods) route through a new optional `ImportHandler` on provider adapters. Claude’s handler builds a pure import plan from explicit source paths, runs copy/scrub on `spawn_blocking` via an in-memory `ImportJobs` registry (sibling to refresh jobs), then persists a `local_import` manifest on the profile. The Swift Import sheet mirrors the Open-session dedicated-window pattern and polls `get_import_job` like refresh.

**Tech Stack:** Rust (tokio, serde, schemars, thiserror, anyhow) in `crates/usage-core` + `crates/usage-daemon`; Swift (SwiftUI/AppKit, XCTest) in `apps/UsageMenuBar`. Verification: `cargo test --workspace`, `swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete`, `cargo clippy --all-targets`, `cargo fmt --all -- --check`.

## Global Constraints

- Local `~/.claude` / `~/.claude.json` are **read-only**; never write to them.
- Managed destination only: `~/.usagetracker/profiles/claude/<id>/`.
- **No `projects/` import. No plugins import.** PR2 toggles that remain false are rejected if the client sends them `true`.
- New Claude profile keys need **dual registration**: `ClaudeProfileSettings` + `ClaudeAdapter::profile_setting_keys()`.
- Protocol v3 lockstep: `supports_method`, schema regen, wire fixtures, Swift `Wire.swift`, `docs/api/methods.md`.
- Commit style: imperative sentence, no conventional-commit prefix (match recent history).
- Do not follow symlinks; reject escape from source root.
- Fixture mode: reject import side effects (`unsupported_operation`); preview may return canned data.
- Suspend the target account’s refresh / CLI-fallback while an import job for that account is `queued`/`running` (stretch-quality: at minimum refuse starting a second import for the same account; prefer also skipping that account in refresh while import is active).

---

## File Structure

**Created (Rust):**
- `crates/usage-daemon/src/providers/claude/local_import.rs` — `plan` / `run`, scrubbers, symlink policy, staging
- `crates/usage-daemon/src/import_jobs.rs` — in-memory `ImportJobs` registry
- `crates/usage-core/wire-fixtures/account_import_preview_v3.json`
- `crates/usage-core/wire-fixtures/import_job_v3.json`

**Modified (Rust):**
- `crates/usage-core/src/ids.rs` — `ImportJobId`
- `crates/usage-core/src/api.rs` — import types, request/response variants, `supports_method`, `ProviderCapabilities.import_account_data`, `ApiErrorCode::UnknownImportJob`
- `docs/api/schemas/v3/request.json`, `response.json` — regenerated
- `crates/usage-daemon/src/providers/claude/mod.rs` — `mod local_import`
- `crates/usage-daemon/src/providers/claude/settings.rs` — `local_import` field
- `crates/usage-daemon/src/providers/claude/adapter.rs` — keys, `ImportHandler`
- `crates/usage-daemon/src/runtime/provider_adapter.rs` — `ImportHandler` trait + capability
- `crates/usage-daemon/src/daemon.rs` — preview/import/get-job + fixture gates
- `crates/usage-daemon/src/server.rs` — request arms
- `crates/usage-daemon/src/lib.rs` or `main` module graph — `mod import_jobs`
- `docs/api/methods.md`, `docs/api/versioning.md`, `docs/claude.md`, `CHANGELOG.md`

**Created (Swift):**
- `…/Networking/Models/AccountImport.swift` — options, mode, preview, job models
- `…/State/ImportLocalClaude.swift` — `ImportLocalClaudeModel`
- `…/Views/Settings/ImportLocalClaudeSheet.swift` — sheet + `ImportLocalClaudeWindow`
- `…/Tests/…/ImportLocalClaudeModelTests.swift`

**Modified (Swift):**
- `Wire.swift`, `DaemonClient.swift`, `AppState.swift`, `Settings.swift`, `ReliabilityTests.swift`

---

### Task 1: usage-core wire types (workspace stays green)

**Files:**
- Modify: `crates/usage-core/src/ids.rs`
- Modify: `crates/usage-core/src/api.rs`
- Modify: `crates/usage-daemon/src/server.rs` (stub match arms so daemon compiles)
- Modify: `crates/usage-daemon/src/runtime/provider_adapter.rs` (`descriptor` capability field)
- Regenerate: `docs/api/schemas/v3/*.json`

**Interfaces:**
- Produces: `ImportJobId`, `ImportMode`, `ImportOptions`, `ImportJobStatus`, `ImportJob`, `AccountImportPreview`, `ImportToggleSize`, request variants `PreviewAccountImport` / `ImportAccountData` / `GetImportJob`, response variants `AccountImportPreview` / `ImportStarted` / `ImportJob`, `ProviderCapabilities.import_account_data`, `ApiErrorCode::UnknownImportJob`

- [ ] **Step 1: Write the failing tests** — append in `crates/usage-core/src/api.rs` `mod tests`:

```rust
    #[test]
    fn import_methods_are_supported_and_round_trip() {
        assert!(ApiRequest::supports_method("preview_account_import"));
        assert!(ApiRequest::supports_method("import_account_data"));
        assert!(ApiRequest::supports_method("get_import_job"));

        let request: RequestEnvelope = serde_json::from_str(
            r#"{"api_version":3,"method":"import_account_data","account_id":"account-1","options":{"prefs":true,"project_trust":true,"prompt_history":true},"mode":"prefs_only"}"#,
        )
        .unwrap();
        let ApiRequest::ImportAccountData {
            account_id,
            options,
            mode,
        } = request.request
        else {
            panic!("unexpected request");
        };
        assert_eq!(account_id.as_str(), "account-1");
        assert!(options.prefs);
        assert!(options.project_trust);
        assert!(options.prompt_history);
        assert!(!options.plugins);
        assert!(!options.project_transcripts);
        assert_eq!(mode, ImportMode::PrefsOnly);

        let response = ResponseEnvelope::new(ApiResponse::ImportStarted {
            job: ImportJob {
                id: ImportJobId::new("import-1"),
                account_id: AccountId::new("account-1"),
                provider_id: ProviderId::new("claude"),
                status: ImportJobStatus::Queued,
                mode: ImportMode::PrefsOnly,
                options: ImportOptions::comfort_defaults(),
                created_at: chrono::DateTime::parse_from_rfc3339("2026-07-22T00:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                started_at: None,
                finished_at: None,
                progress_message: None,
                failure_message: None,
            },
        });
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["type"], "import_started");
        assert_eq!(value["job"]["status"], "queued");
        assert_eq!(value["job"]["mode"], "prefs_only");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p usage-core import_methods_are_supported -- --nocapture 2>&1 | tail -20`  
Expected: FAIL (missing types / methods)

- [ ] **Step 3: Implement wire types**

In `ids.rs` add: `string_id!(ImportJobId);`

In `api.rs` after `LaunchFlags` / near refresh types, add:

```rust
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    #[default]
    PrefsOnly,
    Replace,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
pub struct ImportOptions {
    #[serde(default = "default_true")]
    pub prefs: bool,
    #[serde(default = "default_true")]
    pub project_trust: bool,
    #[serde(default = "default_true")]
    pub prompt_history: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub plugins: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub project_transcripts: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub file_history: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tasks_teams: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sessions: bool,
}

impl ImportOptions {
    pub fn comfort_defaults() -> Self {
        Self {
            prefs: true,
            project_trust: true,
            prompt_history: true,
            plugins: false,
            project_transcripts: false,
            file_history: false,
            tasks_teams: false,
            sessions: false,
        }
    }

    /// PR2 rejects any stretch toggle that would import plugins/transcripts/etc.
    pub fn ensure_pr2_supported(&self) -> Result<(), String> {
        if self.plugins {
            return Err("plugin import is not supported yet".into());
        }
        if self.project_transcripts {
            return Err("project transcript import is not supported yet".into());
        }
        if self.file_history {
            return Err("file-history import is not supported yet".into());
        }
        if self.tasks_teams {
            return Err("tasks/teams import is not supported yet".into());
        }
        if self.sessions {
            return Err("sessions import is not supported yet".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

impl ImportJobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ImportJob {
    pub id: ImportJobId,
    pub account_id: AccountId,
    pub provider_id: ProviderId,
    pub status: ImportJobStatus,
    pub mode: ImportMode,
    pub options: ImportOptions,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ImportToggleSize {
    pub key: String,
    pub enabled_by_default: bool,
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct AccountImportPreview {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub source_home: String,
    pub source_claude_json: String,
    pub destination: String,
    pub has_managed_config_dir: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_identity: Option<String>,
    pub default_mode: ImportMode,
    pub default_options: ImportOptions,
    pub toggles: Vec<ImportToggleSize>,
}
```

Extend `ApiRequest` with:

```rust
    PreviewAccountImport {
        account_id: AccountId,
    },
    ImportAccountData {
        account_id: AccountId,
        #[serde(default)]
        options: ImportOptions,
        #[serde(default)]
        mode: ImportMode,
    },
    GetImportJob {
        job_id: ImportJobId,
    },
```

Add those three strings to `supports_method`.

Extend `ApiResponse` with:

```rust
    AccountImportPreview {
        preview: AccountImportPreview,
    },
    ImportStarted {
        job: ImportJob,
    },
    ImportJob {
        job: ImportJob,
    },
```

Add `import_account_data: bool` to `ProviderCapabilities` with `#[serde(default, skip_serializing_if = "is_false")]`.

Add `UnknownImportJob` to `ApiErrorCode` with wire string `"unknown_import_job"`.

Update `ids` import at top of `api.rs` to include `ImportJobId`.

In `provider_adapter.rs` `descriptor()`, set `import_account_data: self.import_handler().is_some()` and add a temporary:

```rust
    fn import_handler(&self) -> Option<&dyn ImportHandler> {
        None
    }
```

Plus a stub trait (methods filled in Task 5):

```rust
#[async_trait]
pub(crate) trait ImportHandler: Send + Sync {
    async fn preview(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
    ) -> anyhow::Result<usage_core::AccountImportPreview>;

    async fn start_import(
        &self,
        runtime: ProviderRuntime<'_>,
        account: Account,
        options: usage_core::ImportOptions,
        mode: usage_core::ImportMode,
    ) -> anyhow::Result<usage_core::ImportJob>;
}
```

In `server.rs` `handle_request`, add match arms that return `UnsupportedOperation` stubs temporarily OR call through daemon methods added as `todo!` — prefer compiling stubs:

```rust
            ApiRequest::PreviewAccountImport { account_id } => {
                match self.runtime.preview_account_import(account_id).await {
                    Ok(preview) => ApiResponse::AccountImportPreview { preview },
                    Err(err) => map_import_error(err),
                }
            }
            ApiRequest::ImportAccountData {
                account_id,
                options,
                mode,
            } => match self
                .runtime
                .import_account_data(account_id, options, mode)
                .await
            {
                Ok(job) => ApiResponse::ImportStarted { job },
                Err(err) => map_import_error(err),
            },
            ApiRequest::GetImportJob { job_id } => {
                match self.runtime.get_import_job(&job_id).await {
                    Ok(Some(job)) => ApiResponse::ImportJob { job },
                    Ok(None) => ApiResponse::error(
                        ApiErrorCode::UnknownImportJob,
                        format!("unknown import job: {}", job_id.as_str()),
                    ),
                    Err(err) => ApiResponse::error(
                        ApiErrorCode::StorageUnavailable,
                        err.to_string(),
                    ),
                }
            }
```

Add matching `preview_account_import` / `import_account_data` / `get_import_job` on `DaemonRuntime` that for now return `anyhow::bail!("not implemented")` so the workspace compiles — Task 5/6 replace them.

Regenerate schemas:

```bash
cargo run -p usage-core --example generate-schemas -- docs/api/schemas/v3
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p usage-core import_methods_are_supported checked_in_protocol_schemas 2>&1 | tail -30`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/usage-core/src/ids.rs crates/usage-core/src/api.rs docs/api/schemas/v3 \
  crates/usage-daemon/src/server.rs crates/usage-daemon/src/runtime/provider_adapter.rs \
  crates/usage-daemon/src/daemon.rs
git commit -m "$(cat <<'EOF'
Add import wire types for comfort-pack jobs

EOF
)"
```

---

### Task 2: Pure Claude local_import planner + prefs/trust/history runner

**Files:**
- Create: `crates/usage-daemon/src/providers/claude/local_import.rs`
- Modify: `crates/usage-daemon/src/providers/claude/mod.rs` — `pub(crate) mod local_import;`

**Interfaces:**
- Consumes: `usage_core::ImportOptions`, `ImportMode`
- Produces: `ImportPlan`, `plan(...)`, `run(...)`, `ImportManifest`, scrub helpers

- [ ] **Step 1: Write failing tests** inside `local_import.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn plan_includes_only_selected_comfort_paths() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("claude-home");
        let json = dir.path().join("claude.json");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("settings.json"), r#"{"theme":"dark","env":{"X":"1"}}"#).unwrap();
        fs::write(home.join("history.jsonl"), "line\n").unwrap();
        fs::write(&json, r#"{"projects":{"/tmp/demo":{"hasTrustDialogAccepted":true,"mcpServers":{}}}}"#).unwrap();

        let plan = plan(&home, &json, &ImportOptions::comfort_defaults()).unwrap();
        assert!(plan.items.iter().any(|i| i.kind == ImportItemKind::Settings));
        assert!(plan.items.iter().any(|i| i.kind == ImportItemKind::History));
        assert!(plan.items.iter().any(|i| i.kind == ImportItemKind::Trust));
        assert!(!plan.items.iter().any(|i| matches!(i.kind, ImportItemKind::Plugins | ImportItemKind::Projects)));
    }

    #[test]
    fn settings_scrub_drops_secrets_and_keeps_comfort_keys() {
        let raw = serde_json::json!({
            "theme": "dark",
            "model": "fable",
            "enabledPlugins": {"x@y": true},
            "env": {"ANTHROPIC_API_KEY": "secret"},
            "apiKeyHelper": "evil",
            "skipDangerousModePermissionPrompt": true
        });
        let scrubbed = scrub_settings_json(&raw).unwrap();
        assert_eq!(scrubbed["theme"], "dark");
        assert_eq!(scrubbed["model"], "fable");
        assert!(scrubbed.get("env").is_none());
        assert!(scrubbed.get("apiKeyHelper").is_none());
        assert!(scrubbed.get("skipDangerousModePermissionPrompt").is_none());
    }

    #[test]
    fn trust_scrub_keeps_only_allowlisted_project_keys() {
        let raw = serde_json::json!({
            "hasCompletedOnboarding": true,
            "projects": {
                "/tmp/demo": {
                    "hasTrustDialogAccepted": true,
                    "hasCompletedProjectOnboarding": true,
                    "mcpServers": {"bad": {}},
                    "lastSessionFirstPrompt": "hello"
                }
            }
        });
        let scrubbed = scrub_claude_json(&raw).unwrap();
        assert_eq!(scrubbed["hasCompletedOnboarding"], true);
        let project = &scrubbed["projects"]["/tmp/demo"];
        assert_eq!(project["hasTrustDialogAccepted"], true);
        assert!(project.get("mcpServers").is_none());
        assert!(project.get("lastSessionFirstPrompt").is_none());
    }

    #[test]
    fn run_prefs_only_writes_scrubbed_files_and_leaves_credentials() {
        let dir = tempdir().unwrap();
        let source_home = dir.path().join("src");
        let source_json = dir.path().join("src.json");
        let dest = dir.path().join("dest");
        fs::create_dir_all(&source_home).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(source_home.join("settings.json"), r#"{"theme":"dark","env":{"X":"1"}}"#).unwrap();
        fs::write(source_home.join("history.jsonl"), "prompt\n").unwrap();
        fs::write(&source_json, r#"{"hasCompletedOnboarding":true,"projects":{"/a":{"hasTrustDialogAccepted":true,"mcpServers":{}}}}"#).unwrap();
        let creds = br#"{"keep":"me"}"#;
        fs::write(dest.join(".credentials.json"), creds).unwrap();

        let plan = plan(&source_home, &source_json, &ImportOptions::comfort_defaults()).unwrap();
        let manifest = run(&plan, &dest, ImportMode::PrefsOnly).unwrap();
        assert!(dest.join("settings.json").exists());
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(dest.join("settings.json")).unwrap()).unwrap();
        assert!(settings.get("env").is_none());
        assert_eq!(fs::read(dest.join(".credentials.json")).unwrap(), creds);
        assert!(manifest.paths.contains(&"settings.json".into()));
        assert!(manifest.paths.contains(&"history.jsonl".into()));
        assert!(manifest.paths.contains(&".claude.json".into()));
    }

    #[test]
    fn rejects_symlink_escape_from_source_root() {
        let dir = tempdir().unwrap();
        let source_home = dir.path().join("src");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&source_home).unwrap();
        fs::write(&outside, "secret").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, source_home.join("settings.json")).unwrap();
            let source_json = dir.path().join("missing.json");
            let err = plan(&source_home, &source_json, &ImportOptions {
                prefs: true,
                project_trust: false,
                prompt_history: false,
                ..ImportOptions::comfort_defaults()
            });
            assert!(err.is_err());
        }
    }
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p usage-daemon plan_includes_only_selected -- --nocapture 2>&1 | tail -20`  
Expected: FAIL (module missing)

- [ ] **Step 3: Implement `local_import.rs`**

Core API shape:

```rust
pub(crate) struct ImportPlan {
    pub source_home: PathBuf,
    pub source_claude_json: PathBuf,
    pub options: ImportOptions,
    pub items: Vec<ImportItem>,
    pub source_identity: Option<String>,
}

pub(crate) enum ImportItemKind { Settings, Trust, History, Plugins, Projects, FileHistory, TasksTeams, Sessions }

pub(crate) struct ImportItem {
    pub kind: ImportItemKind,
    pub relative_dest: PathBuf, // e.g. settings.json, .claude.json, history.jsonl
    pub estimated_bytes: u64,
}

pub(crate) struct ImportManifest {
    pub paths: Vec<String>,
    pub imported_at: DateTime<Utc>,
}

pub(crate) fn plan(
    source_home: &Path,
    source_claude_json: &Path,
    options: &ImportOptions,
) -> anyhow::Result<ImportPlan>

pub(crate) fn run(
    plan: &ImportPlan,
    dest_config_dir: &Path,
    mode: ImportMode,
) -> anyhow::Result<ImportManifest>
```

Implementation rules:
- Settings allowlist keys (initial): `model`, `theme`, `enabledPlugins`, `extraKnownMarketplaces`, `switchModelsOnFlag` (extend only with clearly non-secret UI prefs found in real settings during impl if needed).
- Trust project keep-keys: `hasTrustDialogAccepted`, `hasCompletedProjectOnboarding`. Top-level: `hasCompletedOnboarding`, `lastOnboardingVersion`.
- History: copy `history.jsonl` with no symlink follow (`symlink_metadata` + reject symlink); use staging file `history.jsonl.importing` then rename.
- Trust: write scrubbed JSON to dest `.claude.json` via staging.
- Settings: write scrubbed JSON to dest `settings.json` via staging.
- `prefs_only` and `replace` for PR2 comfort paths behave the same for these three files (overwrite those paths). For `replace`, also delete previously-manifested paths that are **not** in the new plan — but never delete `.credentials.json`, identity files, or paths outside the prior manifest.
- Source identity: best-effort read of source home’s cached oauth profile JSON if present (reuse `parse_cached_profile_identity` patterns / path `~/.claude` typically has no oauth file — try `source_home` parent `.claude.json` oauth fields or skip). Prefer scanning source `.claude.json` for a non-secret email-like field only if already present in known shapes; otherwise `None` is fine.
- Free-space preflight: require dest filesystem free space ≥ sum(estimated_bytes) + 1 MiB.
- Copy: `std::fs::copy` is fine for PR2 (clonefile optional stretch); must not follow symlinks.
- Live source `ENOENT` mid-copy: skip that item and record in progress, do not hard-fail the whole job unless zero items succeeded and at least one was selected.

Export `estimate_toggle_bytes(source_home, source_claude_json, options) -> Vec<(key, Option<u64>)>` used by preview.

- [ ] **Step 4: Run tests**

Run: `cargo test -p usage-daemon local_import 2>&1 | tail -40`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/usage-daemon/src/providers/claude/local_import.rs crates/usage-daemon/src/providers/claude/mod.rs
git commit -m "$(cat <<'EOF'
Add Claude comfort-pack local import planner

EOF
)"
```

---

### Task 3: `local_import` profile settings + key parity

**Files:**
- Modify: `crates/usage-daemon/src/providers/claude/settings.rs`
- Modify: `crates/usage-daemon/src/providers/claude/adapter.rs` (`profile_setting_keys` + parity test already exists — extend)

**Interfaces:**
- Produces: `ClaudeLocalImportSettings` nested under `ClaudeProfileSettings.local_import`

- [ ] **Step 1: Failing test** — extend parity / round-trip in settings or adapter tests:

```rust
    #[test]
    fn local_import_settings_round_trip_in_profile() {
        let settings = ClaudeProfileSettings {
            local_import: Some(ClaudeLocalImportSettings {
                last_imported_at: Some(chrono::Utc::now()),
                source: Some("~/.claude".into()),
                source_identity: Some("user@example.com".into()),
                options: Some(ImportOptions::comfort_defaults()),
                manifest: Some(ClaudeImportManifest {
                    paths: vec!["settings.json".into()],
                    imported_at: chrono::Utc::now(),
                }),
            }),
            ..ClaudeProfileSettings::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        let back: ClaudeProfileSettings = serde_json::from_value(value).unwrap();
        assert!(back.local_import.is_some());
    }
```

Also assert `"local_import"` is in `ClaudeAdapter.profile_setting_keys()` via existing parity test once the field exists.

- [ ] **Step 2: Implement structs** in `settings.rs`:

```rust
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaudeLocalImportSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_imported_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) options: Option<ImportOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) manifest: Option<ClaudeImportManifest>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaudeImportManifest {
    pub(crate) paths: Vec<String>,
    pub(crate) imported_at: DateTime<Utc>,
}
```

Add field `local_import: Option<ClaudeLocalImportSettings>` to `ClaudeProfileSettings`.  
Add `"local_import"` to `profile_setting_keys()`.

- [ ] **Step 3: Tests pass + commit**

```bash
cargo test -p usage-daemon local_import_settings profile_setting_keys_cover 2>&1 | tail -20
git add crates/usage-daemon/src/providers/claude/settings.rs crates/usage-daemon/src/providers/claude/adapter.rs
git commit -m "$(cat <<'EOF'
Persist Claude local_import profile settings

EOF
)"
```

---

### Task 4: ImportJobs registry

**Files:**
- Create: `crates/usage-daemon/src/import_jobs.rs`
- Modify: crate root module (`crates/usage-daemon/src/lib.rs` or `main.rs` / `daemon` module tree) to `mod import_jobs;`
- Modify: `DaemonRuntime` to hold `Arc<ImportJobs>`

**Interfaces:**
- Produces: `ImportJobs::start`, `get`, retention 64, per-account single active import

- [ ] **Step 1: Failing tests** in `import_jobs.rs`:

```rust
    #[tokio::test]
    async fn start_returns_queued_job_and_get_reads_it() {
        let jobs = ImportJobs::new();
        let job = jobs
            .start(sample_job(ImportJobStatus::Queued), || async {
                Ok::<(), anyhow::Error>(())
            })
            .await
            .unwrap();
        assert_eq!(job.status, ImportJobStatus::Queued);
        let fetched = jobs.get(&job.id).await.unwrap();
        assert_eq!(fetched.id, job.id);
    }

    #[tokio::test]
    async fn rejects_second_active_import_for_same_account() {
        let jobs = ImportJobs::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let gate2 = gate.clone();
        let _first = jobs
            .start(sample_job(ImportJobStatus::Queued), move || {
                let gate2 = gate2.clone();
                async move {
                    gate2.notified().await;
                    Ok(())
                }
            })
            .await
            .unwrap();
        let err = jobs
            .start(sample_job(ImportJobStatus::Queued), || async { Ok(()) })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already"));
        gate.notify_one();
    }
```

- [ ] **Step 2: Implement** a focused registry (do **not** copy all of `RefreshCoordinator`):

```rust
pub struct ImportJobs { /* Mutex<Inner> with by_id, active_by_account, completed deque */ }

impl ImportJobs {
    pub fn new() -> Self;
    pub async fn start<F, Fut>(&self, job: ImportJob, work: F) -> anyhow::Result<ImportJob>
    where F: FnOnce() -> Fut + Send + 'static, Fut: Future<Output = anyhow::Result<()>> + Send + 'static;
    pub async fn get(&self, id: &ImportJobId) -> Option<ImportJob>;
    pub async fn account_has_active_import(&self, account_id: &AccountId) -> bool;
}
```

On start: insert queued job, spawn task that sets Running, runs work, sets Completed/Failed with timestamps + failure_message, retains last 64.

Wire `Arc<ImportJobs>` into `DaemonRuntime` construction (both normal and fixture constructors).

- [ ] **Step 3: Tests + commit**

```bash
cargo test -p usage-daemon import_jobs 2>&1 | tail -30
git add crates/usage-daemon/src/import_jobs.rs crates/usage-daemon/src/*.rs
git commit -m "$(cat <<'EOF'
Add in-memory import job registry

EOF
)"
```

---

### Task 5: ImportHandler on ClaudeAdapter + daemon methods

**Files:**
- Modify: `crates/usage-daemon/src/runtime/provider_adapter.rs` (complete trait if needed)
- Modify: `crates/usage-daemon/src/providers/claude/adapter.rs`
- Modify: `crates/usage-daemon/src/daemon.rs`
- Modify: `crates/usage-daemon/src/server.rs` (error mapping polish)

**Interfaces:**
- Consumes: Task 2 plan/run, Task 4 ImportJobs, managed profile paths
- Produces: working `preview_account_import` / `import_account_data` / `get_import_job`

- [ ] **Step 1: Failing integration-style tests** in `adapter.rs` or `daemon.rs`:

```rust
    #[tokio::test]
    async fn import_capability_is_claude_only() {
        let ids: Vec<_> = provider_registry::descriptors()
            .into_iter()
            .filter(|d| d.capabilities.import_account_data)
            .map(|d| d.id.to_string())
            .collect();
        assert_eq!(ids, vec!["claude".to_string()]);
    }

    // Use temp HOME / explicit source dirs via env or by calling local_import directly
    // through the handler with a managed profile fixture — follow existing daemon test
    // helpers for storage + config with a managed claude profile.
```

Also test: fixture mode rejects import; preview allowed with canned or real read-only preview; unsupported stretch options → `invalid_argument`.

- [ ] **Step 2: Implement Claude `ImportHandler`**

Resolve:
- `source_home` = `dirs::home_dir()?.join(".claude")` (only at handler edge — `local_import::plan` still takes explicit paths)
- `source_claude_json` = `dirs::home_dir()?.join(".claude.json")`
- `destination` = expanded managed `claude_config_dir` (require `managed_profiles::is_managed_profile`)

`preview`: build toggle list with defaults + `supported` flags (prefs/trust/history true; others false with notes); estimate bytes via metadata when cheap; include `source_identity` when known; `has_managed_config_dir`.

`start_import`:
1. `options.ensure_pr2_supported()?` → map to invalid_argument
2. require managed config dir
3. `plan` then `ImportJobs::start` with `spawn_blocking` calling `run`
4. On success, update profile `local_import` settings (manifest, options, timestamps, source paths) via config mutate + persist (follow launch-prefs persist pattern in adapter)
5. While job active, `account_has_active_import` should cause refresh path for that account to skip if easy to hook; otherwise document and enforce single-flight only

Daemon methods:

```rust
pub async fn preview_account_import(&self, account_id: AccountId) -> anyhow::Result<AccountImportPreview>
pub async fn import_account_data(&self, account_id: AccountId, options: ImportOptions, mode: ImportMode) -> anyhow::Result<ImportJob>
pub async fn get_import_job(&self, job_id: &ImportJobId) -> anyhow::Result<Option<ImportJob>>
```

Fixture: `import_account_data` bails; `preview_account_import` returns a canned preview for Claude managed accounts (or still computes read-only preview — either is fine; prefer real read-only preview so `just fixture` is useful).

Map errors:
- unknown account → server already has pattern
- bad options / not managed → `invalid_argument` or `unsupported_operation` per design table
- Use a small `InvalidImportRequest(String)` analogous to `InvalidLaunchRequest` if needed

- [ ] **Step 3: Tests + commit**

```bash
cargo test -p usage-daemon import_capability preview_account import_account 2>&1 | tail -40
git add crates/usage-daemon/src/providers/claude/adapter.rs crates/usage-daemon/src/daemon.rs \
  crates/usage-daemon/src/server.rs crates/usage-daemon/src/runtime/provider_adapter.rs
git commit -m "$(cat <<'EOF'
Wire Claude ImportHandler into the daemon API

EOF
)"
```

---

### Task 6: Wire fixtures + API docs

**Files:**
- Create fixtures under `crates/usage-core/wire-fixtures/`
- Modify: `api.rs` round-trip list
- Modify: `docs/api/methods.md`, `docs/api/versioning.md`, `docs/claude.md`, `CHANGELOG.md`
- Ensure schemas already regenerated (Task 1); re-run if types drifted

- [ ] **Step 1: Add fixtures**

`account_import_preview_v3.json` and `import_job_v3.json` mirroring the new response shapes (stable timestamps).

Add both to `wire_fixtures_round_trip_without_losing_contract_fields`.

- [ ] **Step 2: Document methods** in `methods.md` (table + detail), mention `import_account_data` capability in `versioning.md`, short importer section in `docs/claude.md`, CHANGELOG under Unreleased.

- [ ] **Step 3: Commit**

```bash
git add crates/usage-core/wire-fixtures docs/api docs/claude.md CHANGELOG.md crates/usage-core/src/api.rs
git commit -m "$(cat <<'EOF'
Document and pin comfort-pack import wire fixtures

EOF
)"
```

---

### Task 7: Swift wire + DaemonClient

**Files:**
- Create: `apps/UsageMenuBar/Sources/UsageMenuBar/Networking/Models/AccountImport.swift`
- Modify: `Wire.swift`, `DaemonClient.swift`, `ReliabilityTests.swift`

**Interfaces:**
- Produces: encode/decode for preview/import/getImportJob; `waitForImport` poll helper; `ProviderCapabilities.importAccountData`

- [ ] **Step 1: Failing Swift tests** in `ReliabilityTests.swift` decoding the new fixtures (copy PR1 launch-settings test style).

- [ ] **Step 2: Implement models + wire cases**

```swift
struct ImportOptions: Codable, Equatable, Sendable { /* prefs, projectTrust, promptHistory, … */ }
enum ImportMode: String, Codable, Sendable { case prefsOnly = "prefs_only"; case replace }
struct ImportJob: Codable, Equatable, Sendable { /* … */; var status: ImportJobStatus }
enum ImportJobStatus: String, Codable, Sendable {
    case queued, running, completed, failed
    var isTerminal: Bool { self == .completed || self == .failed }
}
struct AccountImportPreview: Codable, Equatable, Sendable { /* … */ }
```

`DaemonRequest` cases + encode; `DaemonResponse` cases + decode; timeouts: preview/getImportJob = 3s, importAccountData = 10s.

`DaemonClient`:

```swift
func previewAccountImport(accountId: String) async throws -> AccountImportPreview
func importAccountData(accountId: String, options: ImportOptions, mode: ImportMode) async throws -> ImportJob
func waitForImport(_ job: ImportJob) async throws -> ImportJob
```

- [ ] **Step 3: `swift test` + commit**

```bash
swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete 2>&1 | tail -40
git add apps/UsageMenuBar
git commit -m "$(cat <<'EOF'
Add Swift wire types for account import jobs

EOF
)"
```

---

### Task 8: Import sheet UI + Settings entry

**Files:**
- Create: `State/ImportLocalClaude.swift`, `Views/Settings/ImportLocalClaudeSheet.swift`, tests
- Modify: `AppState.swift`, `Settings.swift`

**Interfaces:**
- Mirrors `OpenSessionWindow` / `OpenSessionModel`

- [ ] **Step 1: Model tests** for toggle defaults from preview, disabled unsupported toggles, wire options mapping.

- [ ] **Step 2: UI**

`ImportLocalClaudeModel`: account title, paths, identity, mode, toggle bools (only prefs/trust/history editable in PR2; others shown disabled with notes), progress/error strings.

`ImportLocalClaudeWindow` + sheet:
- Show source / destination / identity
- Mode picker (Prefs only / Replace imported paths)
- Toggles
- Warning copy: local Claude is read-only; close any Claude using this managed profile before replace
- Buttons: Import / Cancel
- On Import: call `importAccountData`, poll until terminal, dismiss on success / show error on failure

Settings `accountMenu`: after Open session, add **Import from local Claude…** gated on `supportsImportAccountData(providerId)`. When tapped: `prepareImportLocalClaude` → present window. (Capability alone is enough; preview’s `has_managed_config_dir == false` should surface as an error state in the sheet.)

- [ ] **Step 3: Tests + commit**

```bash
swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete 2>&1 | tail -40
git add apps/UsageMenuBar
git commit -m "$(cat <<'EOF'
Present the Import from local Claude sheet

EOF
)"
```

---

### Task 9: Final verification gate

- [ ] **Step 1: Full Rust + Swift + clippy/fmt**

```bash
cargo test --workspace 2>&1 | tail -50
cargo clippy --all-targets -- -D warnings 2>&1 | tail -40
cargo fmt --all -- --check
swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete 2>&1 | tail -40
```

- [ ] **Step 2: Fix any fallout; commit only if needed**

- [ ] **Step 3: Spec coverage self-check**
  - Job-shaped API ✅
  - ImportHandler + capability ✅
  - prefs/trust/history only ✅
  - no projects/plugins ✅
  - staging + no symlink follow ✅
  - fixture reject import ✅
  - Swift Import sheet ✅
  - dual registration `local_import` ✅
  - wire lockstep ✅

---

## Spec coverage checklist

| Spec requirement | Task |
| --- | --- |
| Job-shaped import API | 1, 4, 5 |
| Generic account-routed methods + ImportHandler | 1, 5 |
| Comfort pack prefs/trust/history | 2 |
| No projects/plugins | 2, 5 (`ensure_pr2_supported`) |
| Staging + symlink reject | 2 |
| `local_import` settings + key parity | 3 |
| Fixture policy | 5 |
| Import sheet | 7–8 |
| Wire artifacts | 1, 6, 7 |
| Suspend/single-flight import vs refresh | 4–5 |

## Out of scope (do not implement)

- Plugin path rewrite, `projects/` + attribution exclusion, import cancel, clonefile-only optimization, auto-import on add, Codex/Grok import
