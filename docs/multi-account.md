# Production multi-account support plan

## Account lifecycle

- Keep account identity stable in SQLite with a durable `accounts.id`.
- Preserve usage history when an account is hidden, disabled, or removed.
- Offer both reversible removal (`hidden = true`, `collection_enabled = false`) and permanent deletion.
- Keep `GetAccounts` administrative and complete so removed accounts can be re-enabled.
- Keep normal usage and health surfaces filtered to visible accounts.

## Collection behavior

- Filter disabled profile-backed accounts at provider configuration time when possible.
- Guard refresh at the storage layer as a second line of defense so rediscovered disabled accounts are skipped.
- Keep provider-wide enablement separate from account-wide enablement.
- Record disabled account health when refresh skips an account.
- Continue to support providers that only expose one web identity by relying on storage-level account lifecycle state.

## Settings UX

- Group provider-wide controls, accounts, and sign-in actions in one provider card.
- Per account, expose:
  - collection on/off,
  - dashboard visibility,
  - remove while keeping history,
  - permanent deletion,
  - restore from a removed state.
- Keep removed accounts in one collapsed cleanup section.
- Keep secondary actions in a single account menu.

## Provider-specific gaps

- Codex: account add/remove should continue to map to isolated profile homes.
- Claude: add UI support for creating and editing named profiles, including `keychain_account`, credential file, CLI toggle, and project roots.
- OpenCode Go: decide whether account-level disable should clear cached cookies, disable only collection, or support named cookie/workspace profiles.
- All providers: expose credential/source diagnostics per account, not only per provider.

## Production hardening

- Add an explicit account rename API and Settings control.
- Add account-level refresh actions.
- Add audit metadata such as `hidden_at`, `disabled_at`, and optional user-facing removal reason.
- Keep irreversible purge behind a second confirmation and delete related snapshots, raw payloads, and health.
- Add migration tests against pre-lifecycle databases.
- Add socket API compatibility tests for older clients missing lifecycle fields.
- Add end-to-end menu bar tests for hide, remove, and re-enable flows.
- Add conflict handling for config-file write failures after database state changes.
