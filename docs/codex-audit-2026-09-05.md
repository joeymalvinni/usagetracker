# UsageTracker running-instance audit — September 5, 2026

## Account investigation

The two Codex profiles have distinct account IDs, user IDs, and login identities. Each auth file's account ID matches the account claim in its tokens. Independent app-server reads, explicitly using each profile's file credentials, returned the expected email for both profiles.

At the initial observation, the main account reported 113,897,797 tokens today and 635,456,491 over 30 days. The second account reported 8,245 today and 58,371 over 30 days. Their lifetime totals were 4,709,402,516 and 1,125,226,155 respectively. These were separate provider responses, not a duplicated dashboard total. The second profile had zero local session files. Its activity therefore came entirely from the provider's account-usage service.

The second account's independent provider response contained these recent daily totals:

| Date | Tokens |
| --- | ---: |
| August 30 | 8,274 |
| August 31 | 8,507 |
| September 1 | 796 |
| September 3 | 797 |
| September 4 | 7,268 |
| September 5 | 8,245 |

OpenAI describes this endpoint as returning ChatGPT account token-activity summaries and daily buckets. It does not return the conversations or devices responsible. These numbers do not establish that the user recently ran a local Codex session using that login. The account has a team plan, but that alone does not establish that these are organization-wide totals. No activity was erased or reassigned based on that assumption. [OpenAI app-server documentation](https://learn.chatgpt.com/docs/app-server#7-token-usage-chatgpt).

## Changes and reasons

### 1. Profile attribution

An explicit `auth_path` was used for discovery and WHAM, but the app-server used `codex_home`. If the home was omitted, it defaulted to the main Codex home even for an unrelated custom auth file. This could attach main-account remote usage and local costs to a second account.

An auth-only profile now derives its home from the auth file's directory. The app-server explicitly uses file credential storage. The collector validates the home account ID before and after remote collection, and before reading local logs. A mismatched home cannot contribute remote or local usage. The existing token-and-account-specific WHAM fallback remains available for custom credentials. Local-only refreshes now perform the same profile identity and duplicate checks as remote refreshes. File watchers use the same home resolution.

This is a confirmed code defect covered by a regression test, but it was not the configuration used by the two live profiles: both already had explicit, distinct homes.

### 2. Codex price catalog

The main profile contained over 121 million Astra tokens and about 30 million GPT-5.1 Codex Mini tokens that the old catalog could not price. The GPT-5.6 catalog also contained outdated rates.

The September 5 catalog uses these USD rates per million tokens:

| Model | Input | Cache read | Cache write | Output |
| --- | ---: | ---: | ---: | ---: |
| GPT-6 Astra | 10 | 1 | 12.50 | 50 |
| GPT-5.6 Sol | 4 | 0.40 | 5 | 20 |
| GPT-5.6 Terra | 2 | 0.20 | 2.50 | 12 |
| GPT-5.6 Luna | 0.20 | 0.02 | 0.25 | 1.20 |
| GPT-5.1 Codex Mini | 0.25 | 0.025 | — | 2 |

For Astra and GPT-5.6, requests exceeding 272,000 input tokens use twice the input/cache rates and 1.5 times the output rate. The `gpt-5.6` alias resolves to Sol. Whitespace, the `openai/` prefix, and dated snapshots normalize correctly. A catalog revision bump invalidates cached per-file costs.

Sources: [Astra](https://developers.openai.com/api/docs/models/gpt-6-astra), [Sol](https://developers.openai.com/api/docs/models/gpt-5.6-sol), [Terra](https://developers.openai.com/api/docs/models/gpt-5.6-terra), [Luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna), [Codex Mini](https://developers.openai.com/api/docs/models/gpt-5.1-codex-mini).

These are current API-equivalent estimates applied across scanned history. They are not subscription charges or invoices reconstructed using historical price schedules. Fast-mode, service-tier, and regional adjustments are not reconstructed by this change.

### 3. Repeated token notifications

The local parser always counted `last_token_usage`, including when a notification repeated unchanged cumulative counters. In September's local files, 26 repeated notifications represented 2,681,758 tokens that the old implementation would recount.

An unchanged cumulative counter now contributes zero. A new request with identical individual usage still counts when the cumulative total increases. Existing handling of total-only baselines, missing dates, and last-only events remains intact. This corrects local token and cost estimates; it does not alter OpenAI's account totals.

### 4. Quota identity and duration

Additional quota windows formerly used array positions such as `codex_additional_0`. Inserting or removing a model limit could associate old observations, forecasts, or hidden-window preferences with a different model.

Additional windows now use an escaped provider metered-feature ID, shared between app-server and WHAM. Known durations determine daily/weekly labels and forecast kinds, including when a primary window is actually weekly. This is a one-time ID transition: additional-limit forecasts need new observations, and an old hidden additional-limit setting may need to be applied again. Main quota IDs are retained.

### 5. App-server reliability and diagnostics

The collector now waits for successful initialization before sending account requests. Login-shell executable discovery has a five-second timeout and bounded output. RPC errors preserve recognizable rate-limited and unauthorized classifications. A rate-limited quota response stops collection instead of immediately requesting WHAM as well, allowing the daemon's existing backoff to apply.

Raw RPC errors and stderr are no longer copied into diagnostic messages. Errors retain the failed operation and normalized failure category. Optional account activity can still fail independently without discarding valid quota data.

### 6. Display accuracy and account switching

Account detail explains whether activity is provider-reported or observed on this Mac. Missing priced local usage displays “Unavailable.” Unknown models display “Price unavailable”; local Codex/Claude costs display “estimated,” instead of implying separate metered and vendor bills. The model list no longer silently stops at five entries.

Switching accounts clears chart hover data and old event rows. Cancelled event requests cannot replace the newly selected account's data or reset its loading state. The model builder has a regression test using the two distinct live token totals, including the second account's missing local cost.

### 7. Stored detail compatibility

Snapshot decoding accepts previous `metadata` and `detail` names while continuing to emit `diagnostics`. This avoids silently dropping cost/activity when reading older snapshots. The pre-existing uncommitted `ProviderUsage` compatibility change was preserved; it also accepts legacy overlay metadata and supplies an empty detail when absent. No database reset or history deletion was needed.

### 8. Claude findings

The enabled Claude account's local history used Opus 5, which was missing from its catalog. Added its published input/output and cache pricing. A separate per-model rollup added one-hour cache writes on top of the total cache-write count even though they were already included. Removed that double count and added a regression test. Unknown Claude models now carry model names in pricing coverage so the UI can identify missing prices. [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing).

### 9. Swift concurrency

Strict-concurrency validation exposed two onboarding warnings. The discovery predicate is now explicitly sendable, and the background refresh explicitly captures the client while retaining a weak app-state reference. This removes the warnings and makes the intended capture behavior explicit.

### 10. Local refresh after an upgrade

Previously, repricing existing local logs depended on either a file change or successful remote collection. The local watcher now schedules an initial reconciliation for enabled local sources. This uses the existing debounce and local-only collection path, so prices can update while remote authentication is unavailable without clearing errors, changing quota freshness, or bypassing provider backoff.

## Verification and limits

The audit covered live account configuration, credential identity comparisons without printing credentials, provider responses, normalized socket data, local token-event structure, SQLite integrity, the app's account carousel, collection/storage/display code, and Rust/Swift tests. Disabled providers were not enabled or authenticated as part of the audit.

The app bundle, configuration, and a consistent SQLite backup were saved under `~/.usagetracker/backups/codex-audit-20260905.udMCbY` before replacing the development build. Provider credentials, account labels, and history were not deleted. The backup contains private local data and should remain local.

Validation completed:

- Rust workspace tests: 397 passed; four performance benchmarks intentionally ignored.
- Swift tests with complete strict-concurrency checking: 66 passed, no warnings in the final Swift build.
- Clippy over all workspace targets/features with warnings treated as errors: passed.
- Rust formatting, patch whitespace checks, app packaging, and strict code-signature verification: passed.
- SQLite `quick_check`: `ok`; the existing schema is version 3 and required no reset.
- Live app-server collection for both Codex profiles succeeded after the update.
- Both Codex profiles report the September 5 catalog. The main profile has zero unpriced tokens across all models observed in its local logs.
- UI verification showed Astra with a nonzero estimate and the real weekly primary limit labeled “Codex weekly.” Switching to the second account showed 8.2K today and 33.9K for seven days. Its cost view showed “Unavailable” and the explanation that priced local usage is absent.
- The new initial local reconciliation repriced Claude to approximately $44.45 over 30 days. Its per-model sum and overall local total both became 52,669,344 tokens. No local usage was removed to achieve that agreement; only the duplicated model-rollup contribution was corrected.

Post-update limitations: Claude's remote refresh returned `keychain_access_failed`; OpenCode Go account discovery timed out. Their previous successful provider snapshots remain available, and local data can now update independently. No Keychain access rules were bypassed or changed. A successful remote verification for those two providers still requires resolving macOS credential access and retrying collection. Both Codex accounts were healthy. The enabled-provider audit cannot establish the origin of the second account's provider-side activity because OpenAI's response has no conversation-level attribution.
