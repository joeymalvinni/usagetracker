# Security

## The trust boundary

UsageTracker is local software. The daemon makes outbound HTTPS and provider-CLI requests, and it exposes a Unix socket to local clients. The default app directory is `0700`; config and socket are `0600`.

There's no extra authentication on the socket. Any process running as the same macOS user can read your usage and diagnostics, change your configuration, manage accounts, acknowledge notifications, and trigger provider login and launch actions. In other words, UsageTracker doesn't try to defend against a malicious process that's *already* running as you.

## Credentials

The daemon may read:

- provider OAuth data from the macOS Keychain or known credential files;
- filtered provider cookies from supported browser stores;
- manual cookie headers from environment variables, files, or `config.json`;
- local provider history files and SQLite databases.

Credentials only ever go to their fixed provider endpoint or provider CLI. On sensitive cookie-backed requests, HTTP redirects are turned off where implemented. Raw provider payloads and secrets aren't intentionally logged or stored in usage snapshots.

A manual `cookie_header` is a secret, stored as plaintext in the owner-only config file. Environment variables can also be read by other processes running as you. When you have the choice, prefer provider login and Keychain-backed sources.

## Diagnostics and permissions

Normalized snapshots can carry sanitized diagnostics — source names, counts, timestamps, model names, plan metadata, and bounded fingerprints. These come back over the socket by default; they aren't a separate, more-privileged surface.

A few things may prompt macOS: browser cookie import can ask for Safe Storage keys, and notification delivery needs notification permission. Login and launch actions you trigger may open Terminal. UsageTracker never needs Screen Recording or Accessibility permission.

When you report a problem, share logs and normalized diagnostics only after checking them for email addresses, account IDs, local paths, workspace IDs, and plan info. Never share cookie headers, bearer tokens, refresh tokens, or credential files.

## What's out of scope

UsageTracker doesn't protect against:

- a compromised same-user process or provider CLI;
- a compromised provider endpoint or account;
- root access, or anyone who can read your Keychain or session;
- tampering with a locally built, ad-hoc-signed development app.
