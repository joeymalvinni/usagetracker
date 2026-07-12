# Grok provider

Grok collection uses two account-wide billing surfaces in order:

1. The official Grok Build CLI's ACP process (`grok --no-auto-update agent stdio`), with the
   `x.ai/billing` extension method.
2. The billing gRPC-web method used by grok.com, authenticated with the existing Grok login
   token and/or a signed-in browser session.

The CLI and ACP transport are documented by xAI, but `x.ai/billing` and the grok.com billing
protobuf are not public API contracts. They are isolated behind small parser modules so a wire
change does not affect account discovery, fallback policy, or the shared daemon protocol.

## Why this order

The CLI is the strongest first choice: it is an official executable, owns refresh of its OAuth
credentials, and returns both the billing period and monetary usage totals. The browser path is a
fallback because it depends on an internal web RPC and cookie formats. Both paths report the same
account-wide allowance, so either result is authoritative; the browser result is not marked as a
local estimate.

`~/.grok/sessions/<encoded-cwd>/<session-id>/signals.json` is scanned over a bounded 30-day
lookback for diagnostics (session count, local tokens, last activity, and models used). It is
intentionally not converted into a quota window. Local token counts cover only this Mac and do not
map to the shared pool's product-weighted usage. A local approximation would make forecasts and
low-usage alerts look more certain than they are. Local signals also never suppress a missing-auth
error when neither account-wide billing source succeeds.

## CLI RPC

`GROK_CLI_PATH` is an authoritative override. Otherwise the daemon resolves `grok` from these
locations before checking the login-shell and process `PATH`:

- `~/.grok/bin/grok`
- `~/.local/bin/grok`
- `/usr/local/bin/grok`
- `/opt/homebrew/bin/grok`

The child uses newline-delimited JSON-RPC with bounded stdout/stderr and a kill-on-drop guard. The
sequence is:

1. `initialize` with ACP protocol version 1 and no filesystem or terminal capabilities.
2. `authenticate` with `cached_token` when advertised. `xai.api_key` is used only when the CLI
   advertises it and `XAI_API_KEY` is present.
3. `x.ai/billing` with no parameters.

Initialization/authentication have a 4-second budget and billing has a 3-second budget. The
daemon disables CLI auto-update for collection so a routine poll cannot mutate the installation.

The included window uses `includedUsed / monthlyLimit`; `totalUsed` is accepted only for older
responses that omit `includedUsed`. Despite the legacy `monthlyLimit` field name, the window kind
comes from the returned billing-cycle duration. An on-demand cap is exposed as a separate window.

## grok.com fallback

The fallback sends an empty gRPC-web protobuf frame to:

`https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig`

It accepts framed and unframed protobuf responses and projects only:

- included usage percent;
- current billing period start, when present;
- next reset time.

The parser selects protobuf field 1 for the percentage rather than accepting an arbitrary float,
ignores trailer frames, validates HTTP and gRPC status, and treats a current period with an omitted
proto3 zero percentage as 0% only when period markers are present.

Transient HTTP 408/502/503/504 failures, gRPC deadline failures, and timeout/connection errors use
a three-attempt bounded retry budget. Authentication and rate-limit failures are never retried with
another source inside the transport.

Fallback is allowed for missing/invalid CLI credentials, unavailable binaries, unsupported RPC
methods, timeouts, and parse failures. A CLI rate limit never triggers the browser path: switching
credentials to bypass provider throttling would be incorrect and could make backoff ineffective.

## Credentials and browser sessions

Identity comes from `$GROK_HOME/auth.json` or `~/.grok/auth.json`. The file is a map keyed by auth
scope; OIDC entries under `https://auth.x.ai::` are preferred over legacy sign-in entries. Access
tokens are never included in raw-payload captures or logs.

For web billing, sources are:

1. `USAGE_TRACKER_GROK_COOKIE`, `providers.grok.cookie_header`, or
   `~/.usagetracker/grok.cookie`;
2. a previously validated browser session cached in macOS Keychain;
3. grok.com sessions imported from Chrome.

Browser database discovery, expiry filtering, and Chromium cookie decryption live in the shared
`providers/browser_cookies.rs` module and are reused by OpenCode. Grok deliberately limits import
to Chrome to avoid unrelated Keychain prompts. Each Chrome profile stays a separate candidate, so
a stale session cannot mask a valid login from another profile. Only `sso` and `sso-rw` are retained
in the outgoing header. Imported sessions have a five-second in-process cache. Imported cookies are
sent only to the fixed HTTPS grok.com endpoint, redirects are disabled, and a cached session is
cleared and re-imported after auth rejection. Unit-test processes do not import browser cookies
unless `USAGE_TRACKER_ALLOW_BROWSER_COOKIE_IMPORT=1` is set.

When a non-expired Grok login token is available, each browser session is tried with the bearer
token and then cookie-only. A bearer-only request is the final web attempt.

## Account model and UI

Grok currently uses one `default` local profile. Before identity is available its external ID is
`grok_default`; storage may atomically adopt the later Grok user ID without creating a duplicate
account. Grok is disabled by default. The Settings action launches `grok login` when installed and
otherwise opens Grok's web usage page.

Collection modes exposed to clients are:

- `grok_cli_billing_rpc`
- `grok_web_billing_rpc`

The optional `providers.grok.source_mode` setting controls strategy selection:

- `auto` (default): CLI followed by web fallback;
- `cli`: CLI only;
- `web`: web only.

## Key modules

- `providers/browser_cookies.rs`: reusable browser discovery and cookie decryption
- `providers/grok/auth.rs`: credential selection and identity
- `providers/grok/rpc.rs`: bounded ACP JSON-RPC transport
- `providers/grok/billing.rs`: provider-neutral billing projection into usage windows
- `providers/grok/web.rs`: gRPC-web validation and protobuf parsing
- `providers/grok/cookies.rs`: Grok-specific source/cache policy
- `providers/grok/local_sessions.rs`: bounded, diagnostic-only local session scan
- `providers/grok/strategy.rs`: typed source-mode policy
- `providers/grok/mod.rs`: account discovery and fallback orchestration
