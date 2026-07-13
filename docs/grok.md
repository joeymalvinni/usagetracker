# Grok

Grok is off by default. You can tell it where to look with `source_mode`: `auto` (try the CLI, then the web), `cli`, or `web`.

## Accounts

Grok supports multiple CLI-backed profiles, each with its own `GROK_HOME`. The default profile uses your normal Grok home and is the only one allowed to use global manual or imported browser cookies. Managed profiles use their own CLI credentials and a bearer-only web fallback, so a single browser session can't accidentally be claimed by several accounts.

The authenticated Grok user ID is the real identity whenever it's available. If two profiles share an ID, the first one wins — though the default profile is allowed to swap its temporary `grok_default` placeholder for the real ID once it learns it.

## Where credentials come from

CLI credentials come from `<grok_home>/auth.json`, preferring complete OIDC entries over legacy ones. Set `GROK_CLI_PATH` to name the executable explicitly; otherwise UsageTracker checks the common install paths, your login shell, and the process `PATH`.

For the default profile's web fallback, cookies are resolved in this order: `USAGE_TRACKER_GROK_COOKIE`, `cookie_header`, `~/.usagetracker/grok.cookie`, the Keychain cache, and finally Chrome's `sso` / `sso-rw` cookies. A Grok bearer token can be used alongside cookies or on its own.

## How usage is collected

1. Run `grok --no-auto-update agent stdio`, set up and authenticate ACP, then call `x.ai/billing`.
2. In `auto` mode, if the CLI fails for any reason other than rate limiting, fall back to `https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig`.
3. A flaky web request gets up to three tries. Authentication and rate-limit failures don't rotate to other credentials mid-request.

The ACP billing extension and the grok.com protobuf are internal surfaces — not stable, public xAI APIs — so they can change without warning.

## How the numbers are normalized

Billing values become included-usage windows, plus an optional on-demand window, using the billing period and reset that the provider returns. Local `signals.json` files are scanned only for 30 days of diagnostic activity — their tokens never become quota, because they only reflect one Mac and don't map to Grok's product-weighted account billing.

## Refresh timing and rate limits

Refreshes happen at most once a minute. A rate limit from either the CLI or the web stops the fallback and starts shared backoff. ACP setup and authentication get four-second budgets; billing gets three. Child-process output is bounded, and the process is killed once collection is done.

## What's kept in diagnostics

Diagnostics can note the collection mode, credential source name, safe identity fields, profile ID and label, billing period and source, and bounded counts, models, and timestamps from local sessions. They never include access or refresh tokens, cookie values, ACP messages, or raw protobuf.

## What failures mean

- Missing binary, login, or cookie → `credentials_missing` or `provider_unavailable`.
- Malformed credentials or manual cookies → `credentials_invalid`.
- Rejected auth → `unauthorized`; a provider throttle → `rate_limited`.
- Process or HTTP transport trouble → `network` or `provider_unavailable`; ACP, protobuf, or billing shapes it can't read → `parse`.
- When both paths fail, the error leans toward authentication and parse details, and safely summarizes both.

## A few security notes

CLI auto-update is disabled while collecting. Managed child processes get their own `GROK_HOME`. Browser requests go to fixed HTTPS endpoints with redirects turned off, and only Grok's authentication cookies are kept. Importing from a browser is limited to the default profile.

## Tests and fixtures

Inline tests cover binary discovery, the ACP/auth/billing exchange, credential precedence, protobuf framing and status, fallback, rate-limit behavior, profile isolation, and local-session bounds. `just fixture` runs normalized Grok data through the socket and UI.

## Known limitations

- Extra browser-only accounts aren't supported, because Grok cookies don't reliably tie a browser session to one identity.
- The CLI and web billing surfaces can change independently of UsageTracker.
- Local session tokens are diagnostic — not account-wide usage or quota.
