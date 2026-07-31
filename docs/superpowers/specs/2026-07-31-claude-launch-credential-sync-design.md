# Claude launch credential sync

Date: 2026-07-31  
Status: draft for review  
Related: `docs/superpowers/specs/2026-07-22-claude-launch-import-design.md` (launch prefs; import does **not** copy OAuth)

## Goal

When the user opens a managed Claude session from UsageTracker, sync that account’s already-stored OAuth into the Keychain item Claude Code reads for that profile’s `CLAUDE_CONFIG_DIR`, so the CLI does not prompt for login every time.

Hard constraints:

- Never put tokens in the generated `.command` launcher, logs, socket payloads, or UI copy.
- Sync only the **selected** account’s credentials into **that** account’s Keychain service.
- Do not share or copy credentials across profiles, and do not pull from the legacy global `Claude Code-credentials` item into a managed profile.
- Build on existing load / refresh / Keychain save paths in `providers/claude/credentials.rs` and the existing Reconnect (`repair`) login flow.

## Decisions

| Topic | Decision |
| --- | --- |
| Sync target | Profile Keychain only (`Claude Code-credentials-<hash>` for the managed config dir) |
| When to sync | Every successful Open Session path for a managed profile (always re-sync) |
| Near-expiry tokens | Refresh with the existing OAuth refresh path before writing |
| Load / sync failure | Do **not** open Terminal; start Reconnect (repair/login) for that account |
| Legacy default (no managed `CLAUDE_CONFIG_DIR`) | Unchanged — no sync step |
| Fixture mode | Unchanged — launch rejected; no Keychain writes |
| Wire API | No new methods — behavior inside `launch_provider_account` |

## User-visible behavior

1. Settings → Claude account → **Open Claude session** (confirm sheet when `launch_options` is advertised).
2. Daemon loads that account’s credentials, refreshes if within the existing skew, re-writes Keychain for the profile service, then opens Terminal with the usual launcher (`CLAUDE_CONFIG_DIR` + flags).
3. If credentials are missing, invalid, Keychain-denied, or refresh is unauthorized: Reconnect starts instead; user finishes browser login; a short action message explains that reconnect was required before opening a session.
4. Success message can remain the existing “Opened a Claude session…” text.

## Architecture

```
Open session (Swift)
  └─ launch_provider_account
       └─ ClaudeAdapter::launch
            ├─ resolve profile + CLAUDE_CONFIG_DIR
            ├─ sync_launch_credentials(profile)   ← new
            │    ├─ load Keychain (file only if Keychain missing)
            │    ├─ refresh if near expiry
            │    ├─ set_if_changed / compare-and-set Keychain
            │    └─ invalidate broker cache for that item
            ├─ on sync failure → repair/login (Reconnect), return action (no Terminal)
            └─ on success → write .command + open Terminal (+ persist launch prefs)
```

### Module boundaries

| Piece | Responsibility |
| --- | --- |
| `providers/claude/credentials.rs` (or small helper next to it) | `sync_for_launch(service, account, credentials_file)` — load, optional refresh, rewrite, cache invalidate. No tokens in returned messages. |
| `providers/claude/adapter.rs` `LaunchHandler::launch` | Call sync for managed profiles before launcher write; map failure to Reconnect via existing repair/login helpers. |
| `providers/launchers.rs` | Unchanged shape — still `unset CLAUDE_SECURESTORAGE_CONFIG_DIR` + `CLAUDE_CONFIG_DIR`; never receives token material. |
| Swift | No new wire types; Reconnect/open messaging via existing action channels. |

### Error mapping

| Case | Behavior |
| --- | --- |
| Credentials missing / invalid / Keychain denied / refresh unauthorized | Start Reconnect; do not open session; `ProviderActionResponse` (or equivalent) with a clear reconnect message |
| Other launch errors (bad cwd, etc.) | Unchanged (`invalid_argument` / existing paths) |
| Fixture launch | Unchanged `unsupported_operation` |

## Security

- Tokens exist only in Keychain (and briefly in daemon memory during load/refresh/write), same as collection today.
- Sync never reads another profile’s Keychain service.
- Comfort-pack import remains credential-free.
- No `.credentials.json` dual-write and no `CLAUDE_CODE_OAUTH_TOKEN` in the launcher (explicit non-goals for this slice).

## Testing

Daemon inline tests:

- Managed launch invokes credential sync before writing/opening the launcher.
- Near-expiry credentials trigger refresh before Keychain write.
- Sync failure starts reconnect and does not call `open_terminal`.
- Identical Keychain contents → write is a no-op (`set_if_changed` / compare-and-set).
- Fixture mode still rejects launch without Keychain mutation.
- Launcher contents still contain no token-like env exports.

Docs: short subsection under `docs/claude.md` (managed launch sync + reconnect-on-failure).

## Non-goals

- Writing `.credentials.json` beside the profile
- Injecting `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` into the `.command` file
- Sharing one Keychain item across multiple managed profiles
- Copying OAuth during comfort-pack import
- Changing Codex/Grok launch auth
- New socket methods or provider capabilities

## Delivery

Single PR: daemon sync + reconnect-on-failure + tests + `docs/claude.md` note. No Swift wire changes required unless copy/UX polish is desired in the same PR.
