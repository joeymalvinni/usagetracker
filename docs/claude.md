# Claude Collection Logic

Claude support lives in `crates/usage-daemon/src/providers/claude/`. It is disabled by default and only runs when `providers.claude.enabled` is true in `config.json`.

Claude supports multiple configured profiles. Existing configs without `providers.claude.profiles` use one legacy profile: the current `USER` Keychain account, `~/.claude/.credentials.json` as the file fallback, and Claude CLI collection enabled.

Accounts added from the menu bar app use isolated directories under `~/.usagetracker/profiles/claude/<profile-id>`. The daemon launches both login and `/usage` with that profile's `CLAUDE_CONFIG_DIR`, so signing in to another account does not replace an existing Claude login.

Local token and cost activity is also profile-scoped. Use **Open Claude session** from the account row in Settings (the terminal icon) to start Claude with that profile's `CLAUDE_CONFIG_DIR`. Claude then writes JSONL history beneath that profile's `projects` directory. The daemon also watches enabled profiles' ordinary JSONL roots. After writes have been quiet for 30 seconds, it refreshes Claude usage, limited to once per minute. A normal `claude` command without the profile environment continues to write under `~/.claude`; only the designated default-activity owner reads that root. Account-wide polling still discovers usage produced on another machine, over SSH, or on the web.

To preserve existing local history during migration, exactly one managed profile may own the shared default roots (`~/.claude/projects` and `~/.config/claude/projects`). When a config has one active managed Claude profile and no existing owner, daemon startup assigns that profile `owns_default_claude_activity: true` and persists the choice. If multiple profiles are active without an owner, the daemon does not guess. Default roots are never assigned to more than one profile, so activity is not duplicated.

Example:

```json
"claude": {
  "enabled": true,
  "profiles": [
    {
      "id": "personal",
      "keychain_account": "your-macos-user",
      "credentials_file": "~/.claude/.credentials.json",
      "cli_enabled": true
    },
    {
      "id": "work",
      "display_name": "Work",
      "claude_config_dir": "~/.claude-work",
      "keychain_account": "your-macos-user",
      "credentials_file": "~/.claude-work/.credentials.json",
      "cli_enabled": true,
      "project_roots": ["~/.claude-work/projects"],
      "owns_default_claude_activity": false
    }
  ]
}
```

For explicit multi-profile configs, `cli_enabled` defaults to true only on the first profile. It permits the Claude CLI fallback when the direct OAuth usage request is unavailable. Managed profiles created by the app configure this automatically.

For a manually configured profile, start interactive sessions with the same directory:

```sh
CLAUDE_CONFIG_DIR=~/.claude-work claude
```

Each collector scans and watches only its enabled profile's configured `project_roots` (or `<claude_config_dir>/projects` when roots are omitted), preventing activity from being duplicated across accounts.

## Credentials

The daemon reads Claude Code OAuth credentials from macOS Keychain first. Legacy profiles use the `Claude Code-credentials` service. Claude Code derives a separate service named `Claude Code-credentials-<hash>` when `CLAUDE_CONFIG_DIR` is set; the daemon uses the same SHA-256-based derivation for isolated profiles. `keychain_service` can override the derived service for a manually configured profile. The Keychain item account is the profile's `keychain_account`, normally the current `USER`.

If the Keychain item does not exist, the daemon falls back to the profile's `credentials_file`, defaulting to `~/.claude/.credentials.json`. Other Keychain errors do not fall back to the file because they may mean the item exists but cannot be read.

The expected credential JSON shape is:

```json
{
  "claudeAiOauth": {
    "accessToken": "access-token",
    "refreshToken": "refresh-token",
    "expiresAt": 1780000000000,
    "scopes": ["user:inference"],
    "subscriptionType": "max",
    "rateLimitTier": "standard"
  }
}
```

`accessToken` and `refreshToken` are required and must be non-empty after trimming. `expiresAt`, `scopes`, `subscriptionType`, and `rateLimitTier` are optional. Invalid JSON, a missing `claudeAiOauth` object, or blank token fields make the provider credentials invalid.

After loading and refreshing credentials, the daemon requests `GET https://api.anthropic.com/api/oauth/profile` and uses the response's `account.uuid` as the stable Claude account identity. The response email is stored separately as account metadata. A configured profile `display_name` remains only a user label, so renaming cannot change identity or bypass duplicate detection. Without a label, storage assigns a generated name such as `Claude 1`.

The same Anthropic account UUID may only be connected once. When multiple profiles authenticate as the same account, the first enabled profile in config order is canonical and later duplicates are not collected. Storage also rejects a profile whose UUID changes, preventing a reconnect from merging two accounts' history. Existing installations are upgraded once from the legacy Keychain-account identity to the canonical UUID; UUID-to-UUID changes remain blocked.

Current Claude Code logins include the `user:profile` OAuth scope. For older tokens without that scope, the daemon may read Claude Code's cached `oauthAccount.accountUuid` from `<CLAUDE_CONFIG_DIR>/.claude.json` (or `~/.claude.json` for the default profile). It never substitutes an email address, macOS username, subscription tier, or display name as account identity. Tokens that declare `user:profile` must successfully resolve the token-bound profile response rather than falling back to potentially stale cached identity.

## Token Refresh

Tokens are considered expired when `expiresAt` is within 60 seconds of the current time. Missing `expiresAt` means the token is treated as not expired until the usage request rejects it.

Refresh requests are sent to:

```text
POST https://platform.claude.com/v1/oauth/token
```

The request is form-encoded with:

```text
grant_type=refresh_token
refresh_token=<stored refreshToken>
client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e
```

The `client_id` is hard-coded, and is Claude CLI's public OAuth client ID. It is the client ID used by Claude Code CLI for OAuth PKCE flow.

The refresh response must include a non-empty `access_token`. If `refresh_token` is missing from the response, the old refresh token is kept. `expires_in` is converted to a new `expiresAt` value in milliseconds. `token_type`, when present, is written back as `tokenType`.

Refreshed credentials are saved back to the same source they came from: Keychain credentials go back into the same profile-specific Keychain item, and file credentials go back to the configured `credentials_file`.

## Usage Request

Usage is fetched from:

```text
GET https://api.anthropic.com/api/oauth/usage
```

The request uses the current access token as a bearer token and sends these headers:

```text
Accept: application/json
anthropic-beta: oauth-2025-04-20
```

If the usage request returns 401 or 403, the daemon refreshes the token once and retries the usage request. Other failed statuses are reported directly. A 429 is treated as rate limiting.

## Usage Normalization

The usage response must be a JSON object. The normalizer recursively searches the payload for utilization windows and also handles `extra_usage`.

Utilization windows are created from objects or values that include utilization-style fields:

```text
utilization
used_percent
usedPercent
percent_used
percentUsed
```

String numbers are accepted. Percent values are clamped to `0..100`. Every percent window gets a limit of `100%`, used percent, remaining percent, and a stable id based on the JSON path.

Reset times are read from any of these fields:

```text
resets_at
resetsAt
reset_at
resetAt
reset_date
resetDate
```

Reset values may be RFC3339 timestamps, `YYYY-MM-DD` dates, Unix seconds, or Unix milliseconds.

Labels are read from:

```text
label
name
title
rate_limit_type
rateLimitType
claim
```

If no label exists, the final path segment is humanized. Labels are prefixed with `Claude` unless they already start with `Claude`.

Window kind is inferred from the window name:

```text
session/hour -> Session
daily/day    -> Daily
weekly/week  -> Weekly
monthly/month -> Monthly
anything else -> Other
```

`extra_usage` is handled separately as a credits window. It accepts usage fields such as `current_usage`, `currentUsage`, `used`, `usage`, `spent`, and `spent_usd`. It accepts limit fields such as `monthly_limit`, `monthlyLimit`, `limit`, `spend_limit`, and `spendLimit`.

If no utilization or extra usage windows can be found, collection fails with a parse error that includes the response top-level keys.

## Snapshot Metadata

Successful terminal snapshots use:

```text
collection_mode: claude_cli_usage
command: claude -p /usage --output-format json --no-session-persistence
reset_text_by_window
```

Successful OAuth API snapshots use:

```text
collection_mode: oauth_usage_api
keychain_service: <legacy or profile-specific service>
keychain_account: <USER or default>
subscription_type
rate_limit_tier
token_expires_at_ms
scopes
extra_usage_enabled
top_level_keys
```

Identity discovery uses the neighboring OAuth profile endpoint. Provider payloads are normalized in
memory and are never persisted.

## Claude CLI Fallback

Claude collection uses the OAuth usage API directly by default. If that request fails for a reason other than rate limiting and `cli_enabled` is true, the collector can fall back to this bounded Claude Code command:

```text
claude -p /usage --output-format json --no-session-persistence
```

For an isolated profile, the subprocess receives that profile's `CLAUDE_CONFIG_DIR`. It also removes proxy environment variables so a daemon-level proxy or connectivity problem does not automatically break the local Claude Code usage lookup. The command must exit successfully and return JSON with a string `result` field.

The CLI path parses usage windows from the print-mode `/usage` result. It recognizes single-line output such as:

```text
Current session: 20% used · resets Jul 7 at 9:39pm (America/Los_Angeles)
Current week (all models): 25% used · resets Jul 7 at 6pm (America/Los_Angeles)
Current week (Fable): 17% used
```

It also accepts the multiline usage shape from the interactive screen:

```text
Current session
20% used
Resets 9:40pm (America/Los_Angeles)
```

Parsed windows are stored as percent windows with stable ids like `claude_cli_usage_current_session`. Reset text is converted to UTC when it uses Claude Code's current formats, including `9:40pm (America/Los_Angeles)` and `Jul 7 at 6pm (America/Los_Angeles)`.

The collector records a warning when this fallback is needed; raw Claude print-mode JSON is never stored.

## Activity-triggered Refresh

JSONL filesystem events are only an activity signal; percentages are never estimated from local token counts. Events are trailing-edge debounced until writes have been quiet for 30 seconds, then coalesced into one OAuth usage refresh. Watcher-triggered refreshes run at most once per minute. The normal configured poll remains the fallback for activity that occurs somewhere the local daemon cannot observe.

Account discovery requires a real Anthropic account UUID from the OAuth profile endpoint or the narrowly scoped legacy cache fallback described above. Missing or invalid credentials no longer create a synthetic account from the current `USER`, because that identity cannot safely distinguish or deduplicate Claude accounts.

## Failure Cases

Missing Keychain credentials and a missing fallback file are reported as missing credentials.

Keychain access errors, invalid JSON, missing OAuth data, blank tokens, and failed credential writes are reported as invalid credentials.

Refresh and OAuth usage request transport failures are network errors. HTTP 401 or 403 means unauthorized, HTTP 429 means rate limited, and other non-success statuses mean the provider is unavailable. A token-refresh HTTP 400 carrying the standard `invalid_grant` code is classified as unauthorized because it indicates a rejected or rotated refresh token.

After a 429, the daemon suppresses more collection attempts for that account with exponential backoff: 5, 10, 20, 40, then at most 60 minutes. Suppressed refreshes report `backing_off` health without calling the provider. A successful collection or provider configuration rebuild clears the backoff.

Usage responses that are not valid JSON, are not objects, or contain no usable windows are parse errors.

The CLI path is provider-unavailable when the `claude` command cannot be spawned, exits unsuccessfully, or times out. CLI output that is invalid JSON or contains no recognizable `/usage` windows is a parse error. If both collection paths fail, the reported error includes both failure messages.

CLI parse failures are retried once after a short delay before the fallback is considered failed.

The daemon logs each stage of this recovery path with structured fields: CLI attempt counts and timings, safe output-shape counters, an output fingerprint, credential source and expiry state, OAuth endpoint/status/error code, and the independent OAuth and CLI error kinds. Raw CLI result text, OAuth response descriptions, access tokens, and refresh tokens are not logged.
