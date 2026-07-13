# Multiple accounts

Codex, Claude, and Grok let you manage as many profiles as you want. OpenCode Go is the exception — it shows one workspace at a time.

## How identity works

- `Account.id` is UsageTracker's own stable ID for managing an account.
- The provider identity (`external_account_id`) is what stops the same real account from being added twice.
- `profile_id` ties an account to its local credentials.
- Display name and email are just for show — they never establish identity.
- If a profile's authenticated provider identity ever changes, it's rejected rather than merged, so two histories never get tangled together.

When two profiles end up pointing at the same provider identity, the first enabled one is the canonical one. That's why the order of your managed profiles matters.

## The account lifecycle

| Action | Visible? | Collecting? | History |
| --- | --- | --- | --- |
| Hide | No | Yes | Kept |
| Disable | Yes | No | Kept |
| Remove | No | No | Kept |
| Restore / show + enable | Yes | Yes | Kept |
| Permanent delete | No | No | Deleted |

`get_accounts` is the administrative view, so it still returns hidden and disabled accounts — that's how you bring them back. Your usage and normal health views leave hidden accounts out.

Permanent deletion clears the stored usage and tombstones (or removes) the profile. Add the account again later and you get a brand-new UsageTracker account — the deleted history doesn't come back.

## Who owns local activity

Separate profile homes keep new local activity apart on their own. But shared roots — like `~/.codex/sessions` and `~/.claude/projects` — can only belong to one profile. UsageTracker records `owns_default_codex_activity` or `owns_default_claude_activity` when it can figure out the owner beyond doubt; when several profiles could plausibly claim the same files, it doesn't guess.

Grok's global browser cookies work the same way: only the default profile gets them. Any additional Grok accounts need their own isolated CLI homes.

For the exact fields involved, see the [configuration reference](configuration.md#provider-fields) and each provider's page.
