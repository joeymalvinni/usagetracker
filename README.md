# usagetracker

Fast local Codex usage tracker.

The CLI reads the Codex auth token from `~/.codex/auth.json` and calls the
ChatGPT/Codex usage endpoints directly:

```text
GET https://chatgpt.com/backend-api/wham/profiles/me
GET https://chatgpt.com/backend-api/wham/usage
Authorization: Bearer <tokens.access_token>
ChatGPT-Account-Id: <tokens.account_id>
User-Agent: codex-cli
```

No `codexbar` subprocess is used. Claude collection is disabled for now.

## Build

```sh
swift build
```

## Run

```sh
swift run usage
swift run usage --table
swift run usage --json
swift run usage --no-refresh
swift run usage refresh
swift run usage daemon --interval=300
swift run usage doctor
```

## State

```text
SQLite DB: ~/Library/Application Support/UsageTracker/usage.sqlite
Config:    ~/.usagetracker/config.json
Auth:      ~/.codex/auth.json
```

Useful overrides:

```sh
CODEX_AUTH_FILE=/tmp/auth.json swift run usage
USAGETRACKER_DB=/tmp/usage.sqlite swift run usage
USAGETRACKER_CONFIG=/tmp/config.json swift run usage
```

## What It Stores

The direct Codex response is normalized into:

- live 5-hour and weekly quota windows from `/backend-api/wham/usage`
- live card rows for today, this week, lifetime tokens, peak day, streak,
  total threads, skills, and most-used reasoning effort from `/backend-api/wham/profiles/me`
- daily usage bucket events in SQLite for history and configured limits
- optional OpenAI organization usage/cost events when `OPENAI_ADMIN_KEY` is set

Generate a starter limits config:

```sh
mkdir -p ~/.usagetracker
swift run usage config-example > ~/.usagetracker/config.json
```


```
AI Usage

┌─ Overview ────────────────────────────────────────────┐
│ Lifetime tokens   1.2B       Peak tokens     208.1M   │
│ Longest task      57m 39s    Current streak  19 days  │
│ Longest streak    19 days                             │
└───────────────────────────────────────────────────────┘

┌─ Activity · last 7 days ──────────────────────────────┐
│ Mon     12M  ░░░░░░░░░░░░                             │
│ Tue     44M  ██░░░░░░░░░░                             │
│ Wed     91M  █████░░░░░░░                             │
│ Thu    208M  ████████████                             │
│ Fri     88M  █████░░░░░░░                             │
│ Sat     53M  ███░░░░░░░░░                             │
│ Sun    156M  █████████░░░                             │
└───────────────────────────────────────────────────────┘

┌─ Codex · openai-web · Pro 5x ─────────────────────────┐
│ Session   67% left  ████████░░░░  resets 2:36 PM      │
│ Weekly    60% left  ███████░░░░░  resets 1:49 PM      │
│ Pace      on track   40% used vs 42% expected         │
│ Forecast  lasts      until reset                      │
│ Credits    0 left    empty                            │
│ Account   joeymalvinni@gmail.com                      │
└───────────────────────────────────────────────────────┘

┌─ Claude 2.1.173 · web ────────────────────────────────┐
│ Session  100% left  ████████████  resets in 4h        │
│ Weekly   100% left  ████████████  resets in 5d 6h     │
│ Pace     under      0% used vs 25% expected           │
│ Forecast lasts      until reset                       │
│ Account  joey_m@clovegrowth.com                       │
└───────────────────────────────────────────────────────┘
```
