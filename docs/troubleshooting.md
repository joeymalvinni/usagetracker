# Troubleshooting

When something looks off, start here:

```sh
cargo run -p usage-cli -- status
```

When the menu app launches the daemon, logs land in `~/.usagetracker/usage-daemon.log` (with numbered rotated archives). A daemon you run in the foreground logs straight to the terminal — add `RUST_LOG=debug` for more detail.

## Opening the unnotarized app

GitHub releases are ad-hoc signed and are not notarized by Apple. Depending on how the app was downloaded, macOS may say that Apple cannot check it for malicious software or that the developer cannot be verified.

First try to open UsageTracker once. If macOS blocks it:

1. Open **System Settings → Privacy & Security**.
2. Scroll to **Security** and find the message about UsageTracker.
3. Click **Open Anyway**, authenticate if asked, and confirm **Open**.

Only approve a copy you intentionally downloaded from the official GitHub repository. Do not disable Gatekeeper globally or run commands that strip security metadata from unrelated apps. The approval is normally required only on first launch after downloading a release.

See [Apple's official app-opening safety guide](https://support.apple.com/102445) for the current macOS instructions and warning meanings.

## Socket problems

| Symptom | What to check |
| --- | --- |
| Socket missing | Start the app or run `cargo run -p usage-daemon`. Make sure the CLI and daemon share the same `USAGE_TRACKER_HOME` or socket override. |
| Connection refused | The file may be stale after a crash. Starting the daemon clears stale socket files automatically — though it won't remove a path that isn't a socket. |
| Another daemon is running | Startup won't take over a socket that's still accepting connections. Stop the existing app/daemon, or use a different socket. |
| Incompatible protocol | Rebuild and upgrade the CLI, app, and daemon together. Only protocol v3 is accepted. |
| Socket path too long | Use a short absolute override like `/tmp/usage-$UID.sock` — macOS caps Unix socket paths via `sockaddr_un`. |
| Permission denied | The default directory should be `0700` and socket/config `0600`. Check ownership before changing modes, and don't make the socket world-readable. |

## Provider problems

| Status | What to do |
| --- | --- |
| `credentials_missing` | Sign in with the provider, or confirm the Keychain/file/browser source you configured actually exists. |
| `credentials_invalid` / `unauthorized` | Run `providers repair PROVIDER`, or repair a specific account with `--account`. |
| `rate_limited` / `backing_off` | Wait it out. Rate limits use the provider's own retry info, or a bounded exponential backoff — hammering refresh won't get you past it. |
| `network` / `provider_unavailable` | Check your connection and whether the provider is up, then refresh. |
| `parse` | The provider's response changed or came back incomplete. Grab your logs, version info, collection mode, and safe diagnostics — never credentials or cookies. |
| `disabled` | Enable the provider or account if you want collection to resume. |

Each provider's sources and fallbacks are spelled out on its own page: [Codex](codex.md), [Claude](claude.md), [OpenCode Go](opencode.md), and [Grok](grok.md).

## Configuration problems

Bad JSON, an unknown field, a poll interval under 60, invalid notification rules, or an unsupported provider will stop the config from loading or updating. Fix the JSON and restart. Since API-driven changes are written atomically, a stray `.tmp` file is safe to remove once you've confirmed no daemon is writing.

## Recovering the database

First, stop the app and daemon. Then back up the database before you do anything else:

```sh
mv ~/.usagetracker/usage.sqlite3 ~/.usagetracker/usage.sqlite3.backup
```

Restarting creates a fresh database. Provider data can be re-collected, but local names, hidden/removed state, notification state, and the local history in the old file won't be rebuilt automatically.

The daemon only ever resets a database it can positively identify as a legacy UsageTracker schema. It refuses to touch a non-empty SQLite file it doesn't recognize — so never point `--db-path` at an unrelated database.
