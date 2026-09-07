# Claude launch prefs + local comfort-pack import

Date: 2026-07-22  
Status: revised after design audit (approve-with-changes)  
Approach: A — extend Claude profile settings + confirm-on-open sheet

## Goal

Let UsageTracker Claude accounts feel like the user’s local Claude Code install when launching sessions, without ever writing to the local Claude home.

Hard constraints:

- Local `~/.claude` and `~/.claude.json` are **read-only** sources.
- Managed profiles stay isolated under `~/.usagetracker/profiles/claude/<id>/`.
- Build on existing profile config, launcher `.command` files, and socket API.
- Clean architecture: importer, settings, launcher, and UI each have one job.
- Do not break usage attribution (imported transcripts must not become that account’s cost/activity).

## Audit disposition

Independent audit verdict: **approve-with-changes**. This revision absorbs the blocker and majors:

| Finding | Resolution in this doc |
| --- | --- |
| Default-ON `projects/` corrupts attribution | `projects/` default **OFF**; transcripts require an exclusion manifest if ever enabled |
| Blocking GB import on serial socket | Job-shaped import API mirroring refresh jobs |
| Provider-specific method names | Account-routed generic methods + optional `ImportHandler` |
| No read surface for Open sheet | `get_account_launch_settings` (or fields on Account) |
| Silent strip via `profile_setting_keys` | Dual registration + parity test; document downgrade loss |
| Invalid `effort: ultracode` example | Closed enum matching CLI (`low`…`max`) |
| Comfort defaults inverted vs real sizes | Prefs/trust/prompt-history on; plugins without cache; projects off |
| Scrub/replace/symlink underspecified | Explicit allowlists, manifest-scoped replace, no symlink follow |
| Open-sheet override skew on old daemon | Capability flag gates the sheet |
| Sticky dangerous flag | Use-once vs save-as-default affordance |

## User-facing features

### 1. Import from local Claude (manual)

Settings → Claude account → **Import from local Claude…**  
(Disabled unless the account has a managed `claude_config_dir`.)

Default checklist (Comfort pack v1):

| Item | Default | Notes |
| --- | --- | --- |
| Prefs (`settings.json`, scrubbed) | on | Allowlisted keys only |
| Project trust (scrubbed `.claude.json` projects map) | on | Enumerated keep-keys only |
| Prompt history (`history.jsonl`) | on | Outside cost-scanned roots; safe |
| Plugins (`plugins/` **excluding** `cache/`) | off in PR2 prefs-only; on later only with path rewrite | Verbatim copy would execute from `~/.claude` — isolation break |
| Project transcripts (`projects/`) | **off** | Attribution risk; stretch only with exclusion manifest |
| `file-history/` | off | |
| `tasks/` / `teams/` | off | |
| `sessions/` | off | |

Always excluded:

- credentials / oauth / Keychain material / `secrets/`
- machine IDs, telemetry caches
- `plugins/cache/` (re-fetchable, ~GB)
- identity-bearing destination files on replace (see Replace semantics)

Preview sheet shows:

- source `~/.claude` (read-only) and destination managed path
- **source account identity** when known (“history created under \<email\>”)
- per-toggle size estimates
- mode: **Replace imported paths** vs **Prefs only**

Import starts a job immediately; UI polls status (`queued` / `running` / `completed` / `failed`). Cancel is stretch.

### 2. Confirm-on-open launch sheet

Settings → Claude account → **Open Claude session** opens a confirm sheet **only when** the server advertises the launch-options capability:

- account identity
- working directory (prefilled from saved prefs; Change folder… via picker)
- launch flags (prefilled): model, effort, dangerously-skip-permissions

Actions: **Open** / **Change folder…** / **Edit flags…** / **Cancel**

Persistence:

- Working directory + model + effort: **save as default** for that account when Open succeeds.
- `dangerously_skip_permissions`: **use once** by default, with an explicit “Remember for this account” checkbox if the user wants it sticky.

Open writes the `.command` launcher and opens Terminal. Missing/invalid cwd → do not launch; surface `invalid_argument`.

## Data model

Extend existing `ClaudeProfileSettings` (optional fields only). New keys **must** also be listed in `ClaudeAdapter::profile_setting_keys()` or an older/current daemon will strip them via `remove_unsupported_settings` and persist the stripped config.

```json
{
  "id": "work",
  "claude_config_dir": "~/.usagetracker/profiles/claude/work",
  "working_directory": "~/Desktop/Notefi",
  "launch": {
    "model": "fable",
    "effort": "xhigh",
    "dangerously_skip_permissions": false
  },
  "local_import": {
    "last_imported_at": "2026-07-22T00:00:00Z",
    "source": "~/.claude",
    "source_identity": "user@example.com",
    "options": {
      "prefs": true,
      "project_trust": true,
      "prompt_history": true,
      "plugins": false,
      "project_transcripts": false,
      "file_history": false,
      "tasks_teams": false,
      "sessions": false
    },
    "manifest": {
      "paths": ["settings.json", "history.jsonl", ".claude.json"],
      "imported_at": "2026-07-22T00:00:00Z"
    }
  }
}
```

### Launch flag validation

| Field | Rule |
| --- | --- |
| `model` | Non-empty constrained string (e.g. alias or full model id); shell-quoted |
| `effort` | Closed enum: `low` \| `medium` \| `high` \| `xhigh` \| `max` (re-verify against target CLI at impl time) |
| `dangerously_skip_permissions` | bool; when true emit bare `--dangerously-skip-permissions` |

Every interpolated path/value goes through `shell_single_quote`.

### Downgrade behavior (honest)

Older daemons do **not** reject unknown profile keys — they strip unsupported keys and may persist the stripped config. Field loss on downgrade is **accepted**; document it. Dual registration + parity tests reduce loss on current builds.

## Architecture

```
Swift Settings UI
  ├─ Import sheet ──► preview_account_import / import_account_data / get_import_job
  └─ Open sheet   ──► get_account_launch_settings + launch_provider_account

Daemon
  ├─ ClaudeProfileSettings (+ profile_setting_keys parity)
  ├─ providers/claude/local_import.rs
  ├─ ImportJobs registry (sibling to RefreshJobs)
  └─ providers/launchers.rs (cwd + flags + cd guard)
```

### Module boundaries

| Module | Responsibility |
| --- | --- |
| `providers/claude/local_import.rs` | Pure `plan(source_home, source_claude_json, options)` / `run(plan, dest, mode)`. Explicit paths (no implicit `~/.claude` resolution) so tests use scratch dirs. Allowlists, scrubs, symlink policy, staging+rename, manifest. |
| `providers/claude/settings.rs` | Optional `working_directory`, `launch`, `local_import` + validation. |
| `providers/claude/adapter.rs` | `profile_setting_keys` parity; `ImportHandler` impl; launch options. |
| `providers/launchers.rs` | `claude_launcher_contents(config_dir, cwd, flags)` with `cd -- '<dir>' \|\| exit 1`. |
| `runtime/provider_adapter.rs` | `LaunchOptions` on `LaunchHandler::launch`; optional `ImportHandler` + derived capability. |
| `ImportJobs` | In-memory job registry next to `RefreshJobs`; copy on `spawn_blocking`. |
| Swift `Wire` / `DaemonClient` / sheet state structs | Sheets from a dedicated window or equivalent that survives `NSOpenPanel` focus (not only the transient popover). |

### API (additive v3, provider-generic)

| Method | Shape |
| --- | --- |
| `get_account_launch_settings` | `{ account_id }` → cwd, launch flags, `has_managed_config_dir`, capability hints |
| `preview_account_import` | `{ account_id }` → paths, default toggles, per-toggle sizes, source identity; size scan off request path |
| `import_account_data` | `{ account_id, options, mode }` → `{ job_id }` immediately |
| `get_import_job` | `{ job_id }` → status / progress / error (mirror `get_refresh_job`) |
| `launch_provider_account` | existing + optional cwd/launch overrides; app only sends overrides when capability present |

Capability bits (follow `setup` precedent), e.g.:

- `launch_options` — Open sheet + launch overrides  
- `import_account_data` — Import menu/sheet  

Wire checklist (must ship in lockstep): `supports_method`, schema regen + `docs/api/schemas/v3`, wire fixtures + round-trips, `docs/api/methods.md`, `Wire.swift` cases.

### Error mapping

| Case | Error |
| --- | --- |
| Missing/invalid cwd or flags | `invalid_argument` |
| No managed config dir / unsupported provider | `unsupported_operation` |
| Fixture mode import/launch side effects | `unsupported_operation` |
| Preview in fixture | May return canned data so `just fixture` can demo the sheet |

## Importer semantics

### settings.json scrub (prefs)

Allowlist comfort keys (initial): `model`, `theme`, `enabledPlugins`, `extraKnownMarketplaces`, `switchModelsOnFlag`, and similar non-secret UI prefs.

Drop or require explicit confirmation (default drop):

- `env` (arbitrary env injection)
- `apiKeyHelper`, `forceLoginMethod`
- `skipDangerousModePermissionPrompt` (security-sensitive)

### Trust scrub (`~/.claude.json` → managed `.claude.json`)

Kept project keys (initial): `hasTrustDialogAccepted`, `hasCompletedProjectOnboarding` (and similarly non-secret onboarding flags as needed).

Excluded by default: `mcpServers` / credential-bearing headers/env, `lastSessionFirstPrompt`, telemetry, and other conversation content.

Top-level comfort allowlist (optional): `hasCompletedOnboarding`, `lastOnboardingVersion` so the managed profile does not re-run first-run — or document that it will if omitted.

### Replace vs prefs_only

- **prefs_only** — overwrite scrubbed prefs (+ trust if selected); leave transcripts/plugins alone.
- **replace** — delete/replace **only paths recorded in the previous import manifest** (or the current plan’s allowlisted paths), never destination identity keys, `.credentials.json`, Keychain-derived state, or natively created sessions outside the manifest.

### Copy mechanics

- Do not follow symlinks; reject targets that escape the source root.
- Staging directory + atomic rename; use names the `*.jsonl` watcher will not treat as live sessions (e.g. `.part` / staging prefix).
- Prefer APFS `clonefile` with byte-copy fallback; free-space preflight.
- Live source: `ENOENT` mid-walk = skip-and-record, not hard failure.
- Suspend the target account’s refresh / CLI-fallback while import runs.
- Warn or refuse if a live Claude session is using the same managed profile (stretch: detect; v1: warn in UI copy).

### Attribution (blocker — explicit)

Managed profiles default `project_roots` to `<claude_config_dir>/projects` and watch `*.jsonl`. Importing `~/.claude/projects/` would:

1. Attribute historical sessions to the managed account’s local activity/cost.
2. Double-count when `owns_default_claude_activity` also scans `~/.claude/projects`.

Therefore:

- **v1 does not import `projects/`.**
- If transcripts ship later: require an import manifest of imported paths + cutoff that the cost/activity scanner uses to **exclude** those files from attribution. Do not ship transcripts without that.

`history.jsonl` is separate (prompt history UI) and is not under cost-scanned project roots.

## Launcher script shape

```sh
#!/bin/zsh -l
unset CLAUDE_SECURESTORAGE_CONFIG_DIR
export CLAUDE_CONFIG_DIR='…'   # or unset for legacy
cd -- '/path/to/workdir' || exit 1
exec claude --model 'fable' --effort 'xhigh'
# optional: --dangerously-skip-permissions
```

`cd` failure must not fall through to `$HOME` with dangerous flags.

## Swift UX notes

- First sheets in this app — treat sheet state as new testable structs; do not claim existing sheet-test infra.
- Prefer presenting Import/Open UI from a surface that survives `NSOpenPanel` key-focus (dedicated window / confirmation flow), not only the transient Settings popover.
- Gate Import on `has_managed_config_dir` + import capability; gate Open-sheet extras on `launch_options`.

## Non-goals

### v1 / PR1–PR2

- Symlinking or sharing one Claude home across accounts
- Continuous sync after import
- Copying OAuth tokens
- Changing Codex/Grok launch flows
- Auto-import on account add
- Project transcript import
- Plugin import without path rewrite
- Import cancel

## Delivery slices

### PR 1 — Launch prefs + confirm-on-open (standalone value)

Settings fields + `profile_setting_keys` parity, launcher cwd/flags + cd guard, `LaunchOptions`, read surface, `launch_options` capability, Swift Open sheet, wire artifacts. **No importer.**

### PR 2 — Comfort import prefs-only

Job-shaped generic import API, importer with settings allowlist + trust scrub + `history.jsonl`, staging/clonefile, fixture policy, Import sheet, wire artifacts. **No `projects/`, no plugins.**

### Stretch (separate design decisions)

- Project transcripts + attribution exclusion manifest  
- Plugins + manifest path rewrite  
- Import cancel, per-toggle size UI polish, live-session guard  

## Testing

Daemon inline tests (extend existing style):

- struct / `profile_setting_keys` parity  
- config downgrade / strip behavior documented  
- importer allowlist; refuses secrets/oauth  
- symlink escape (to excluded path and out of source root)  
- hostile path traversal in project keys  
- replace vs prefs_only; destination identity/credentials byte-identical after both  
- crash mid-import leaves prior state; re-import converges  
- concurrent refresh sees no partial import  
- live-source ENOENT skip  
- launcher cwd + flags + cd guard content  
- missing cwd → `invalid_argument`  
- settings.json scrub  
- fixture-mode rejection for import  
- wire: `supports_method`, schema pin, fixture round-trip  

Swift: `DaemonClient`-style wire tests (LockedTransport). Sheet-state unit tests are new infra, not an extension of nonexistent UI sheet tests.

## Rollout checklist

1. PR1 daemon + Swift Open sheet + protocol artifacts  
2. PR2 importer + Import sheet + protocol artifacts  
3. Docs: `docs/claude.md`, `docs/api/methods.md`, Settings help copy  
4. Re-verify `--effort` / model flag names against the target Claude CLI version at implementation time  

## Open decisions resolved

| Topic | Decision |
| --- | --- |
| Import trigger | Manual Settings button |
| Import v1 contents | Scrubbed prefs + trust + `history.jsonl` |
| Project transcripts | Off until attribution manifest exists |
| Plugins | Off until path rewrite exists; never copy `cache/` |
| Working directory | Per account; confirm/change on every Open |
| Launch flags | Structured; effort closed enum; dangerous flag use-once by default |
| Local Claude | Read-only |
| API shape | Generic account-routed + jobs + capabilities |
| Architecture | Extend profile settings + existing launcher path |
