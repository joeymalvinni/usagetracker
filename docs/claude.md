# Claude Collection Logic

Claude support lives in `crates/usage-daemon/src/providers/claude/`. It is disabled by default and only runs when `providers.claude.enabled` is true in `config.json`.

## Credentials

The daemon reads Claude Code OAuth credentials from macOS Keychain first. The Keychain service is hard-coded as `Claude Code-credentials`. The account name is the current `USER` environment variable, falling back to `default` if `USER` is missing.

If the Keychain item does not exist, the daemon falls back to `~/.claude/.credentials.json` for Linux systems. Other Keychain errors do not fall back to the file because they may mean the item exists but cannot be read.

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

The discovered Claude account id is the Keychain account name, not an Anthropic account id from the payload. The display name is `Claude <subscriptionType>` when a subscription type exists, otherwise `Claude`.

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

Refreshed credentials are saved back to the same source they came from: Keychain credentials go back into the same Keychain item, and file credentials go back to `~/.claude/.credentials.json`.

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
keychain_service: Claude Code-credentials
keychain_account: <USER or default>
subscription_type
rate_limit_tier
token_expires_at_ms
scopes
extra_usage_enabled
top_level_keys
```

Raw provider payloads are only stored when `debug_capture_raw_payloads` is enabled.

## Claude CLI Usage

Claude collection defaults to a bounded Claude Code CLI usage command. It runs:

```text
claude -p /usage --output-format json --no-session-persistence
```

The subprocess removes proxy environment variables so a daemon-level proxy or connectivity problem does not automatically break the local Claude Code usage lookup. The command must exit successfully and return JSON with a string `result` field.

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

The collector adds no warning when this default path succeeds. If `debug_capture_raw_payloads` is enabled, the raw Claude print-mode JSON is stored.

## OAuth API Fallback

If the CLI usage path fails during collection, the daemon tries the OAuth usage API. The collector adds a warning noting that terminal usage failed and the OAuth API fallback was used.

Account discovery also falls back to a generic Claude account using the current `USER` account name when OAuth credentials are missing or invalid. This lets collection use the CLI path on machines where Claude Code itself is logged in but the daemon cannot read or parse the OAuth credential store.

## Failure Cases

Missing Keychain credentials and a missing fallback file are reported as missing credentials.

Keychain access errors, invalid JSON, missing OAuth data, blank tokens, and failed credential writes are reported as invalid credentials.

Refresh and OAuth usage request transport failures are network errors. HTTP 401 or 403 means unauthorized, HTTP 429 means rate limited, and other non-success statuses mean the provider is unavailable.

Usage responses that are not valid JSON, are not objects, or contain no usable windows are parse errors.

The CLI path is provider-unavailable when the `claude` command cannot be spawned, exits unsuccessfully, or times out. CLI output that is invalid JSON or contains no recognizable `/usage` windows is a parse error. If both CLI collection and the OAuth fallback fail, the reported error includes both failure messages.
