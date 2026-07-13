# Configuration reference

The daemon creates `~/.usagetracker/config.json` for you, with `0600` permissions. It rejects any top-level, provider, or profile fields it doesn't recognize. Changes you make through the app or the socket API take effect immediately and are written atomically; if you edit the file by hand, restart the daemon to pick them up.

## What wins when settings conflict

From highest priority to lowest:

1. Daemon command-line flags beat the path environment variables.
2. `USAGE_TRACKER_CONFIG`, `USAGE_TRACKER_DB`, and `USAGE_TRACKER_SOCKET` beat paths derived from `USAGE_TRACKER_HOME`.
3. `USAGE_TRACKER_HOME` beats the default `~/.usagetracker` directory.
4. `USAGE_TRACKER_POLL_INTERVAL_SECONDS` beats the file's polling interval at startup.
5. Provider credential sources follow each [provider's own page](index.md#providers).

## Top-level fields

| Field | Default | Rules |
| --- | --- | --- |
| `poll_interval_seconds` | `300` | A whole number, at least `60`. |
| `notifications` | See below | Your notification policy. |
| `providers` | Codex on, the rest off | A map keyed by `codex`, `claude`, `opencode_go`, or `grok`. Any supported provider you leave out is added at startup. |

The old `debug_capture_raw_payloads` field is still accepted so old files load, but it does nothing and gets removed the next time the file is written.

## Notifications

| Field | Default | Rules |
| --- | --- | --- |
| `enabled` | `true` | The master switch. |
| `thresholds_percent_remaining` | `[50,25,10,5,0]` | 1–7 unique whole numbers from 0 to 100. |
| `reset_alerts` | `true` | Let you know after an authoritative window resets. |
| `predictive_alerts` | `false` | Allow forecast-based alerts. |
| `cooldown_minutes` | `15` | Up to 10,080 (seven days). |
| `quiet_hours` | (none) | `{start_hour_local,end_hour_local}`; hours run 0–23 and must differ. |
| `rules` | `[]` | Per-account or per-window overrides. |

Each rule needs an `account_id`, a `window_id`, or both. From there, optional `enabled`, thresholds, reset/predictive switches, and an RFC 3339 `snoozed_until` override the global policy just for that target.

## Provider fields

| Field | Applies to | What it does |
| --- | --- | --- |
| `enabled` | all | Turns on collection for the provider. |
| `profiles` | Codex, Claude, Grok | Your account profiles, in order. On a duplicate identity, the first one wins. |
| `cookie_header` | OpenCode Go, Grok default profile | A manual cookie override. Stored as plaintext in this private file. |
| `workspace_id` | OpenCode Go | A specific workspace; must start with `wrk_`. Leave it out for automatic discovery. |
| `source_mode` | Grok | `auto`, `cli`, or `web`. Other providers ignore it. |

Codex, Claude, and Grok deliberately share the same set of profile fields:

| Field | What it does |
| --- | --- |
| `id` | A stable profile ID. Managed profiles get one automatically. |
| `enabled` | Whether this profile collects; defaults to `true`. |
| `deleted` | A tombstone left behind after permanent deletion. |
| `display_name` | A local label — never a provider identity. |
| `auth_path`, `codex_home` | Codex's credential file or profile home. |
| `keychain_account`, `keychain_service`, `credentials_file` | Claude credential-source overrides. |
| `claude_config_dir`, `project_roots`, `cli_enabled` | Claude's profile isolation, local activity roots, and CLI fallback. |
| `grok_home` | Grok's profile home. |
| `owns_default_codex_activity`, `owns_default_claude_activity` | Which profile owns the shared default local logs. Migration logic usually manages this for you. |

## Example

```json
{
  "poll_interval_seconds": 300,
  "notifications": {
    "enabled": true,
    "thresholds_percent_remaining": [25, 10, 0]
  },
  "providers": {
    "codex": {
      "enabled": true,
      "profiles": [
        { "id": "personal", "display_name": "Personal", "codex_home": "~/.codex" },
        { "id": "work", "display_name": "Work", "codex_home": "~/.codex-work" }
      ]
    },
    "claude": { "enabled": false },
    "opencode_go": { "enabled": false },
    "grok": { "enabled": false, "source_mode": "auto" }
  }
}
```

Paths starting with `~` are expanded by provider code. Relative override paths are resolved from the daemon's working directory, so absolute paths are the safer choice.

## Environment variables

| Variable | What it's for |
| --- | --- |
| `USAGE_TRACKER_HOME` | The base directory for all default paths. |
| `USAGE_TRACKER_CONFIG`, `USAGE_TRACKER_DB`, `USAGE_TRACKER_SOCKET` | Override individual daemon paths. |
| `USAGE_TRACKER_LOG_LEVEL` | The tracing filter; defaults to `info`. `RUST_LOG` wins when it's valid. |
| `USAGE_TRACKER_POLL_INTERVAL_SECONDS` | Override the polling interval at startup. |
| `USAGE_TRACKER_OPENCODE_GO_COOKIE` | A manual OpenCode cookie header. The legacy `USAGE_TRACKER_OPENCODE_COOKIE` also works. |
| `USAGE_TRACKER_OPENCODE_GO_WORKSPACE_ID` | Override the OpenCode workspace. |
| `USAGE_TRACKER_GROK_COOKIE` | A manual Grok cookie header for the default profile. |
| `GROK_CLI_PATH` | The exact path to the Grok executable. |
| `CODEX_HOME`, `GROK_HOME` | Legacy/default provider homes, outside managed profiles. |
| `CLAUDE_CONFIG_DIR` | The default local Claude activity root; managed profiles set their own child environment. |

There are a few development and test controls too — `USAGE_TRACKER_FIXTURE`, `USAGE_TRACKER_DAEMON` (override the daemon executable the menu app launches), and `USAGE_TRACKER_ALLOW_BROWSER_COOKIE_IMPORT`. The CLI's display variables live in the [CLI reference](cli.md#global-options).
