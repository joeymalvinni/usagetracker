# UsageTracker documentation

Developer guides: [Adding a provider](adding-a-provider.md) · [Maintaining model pricing](model-pricing.md) · [Proposed CLI interface](cli-interface-spec.md)

New here? Start with the [README](../README.md) — it walks you through building the app, running the daemon and CLI, and making your first request.

When you need the details, these pages have them.

## References

- [CLI](cli.md) — every command, its JSON output, exit codes, and how it behaves in scripts.
- [Configuration](configuration.md) — config fields, defaults, overrides, and how live updates work.
- [Troubleshooting](troubleshooting.md) — fixing startup, socket, sign-in, log, and database problems.
- [Security](security.md) — the trust boundary, how credentials are handled, file permissions, and what's out of scope.
- [Data and privacy](data-and-privacy.md) — what's stored, how long it's kept, and how to delete it.
- [Socket API](api/index.md) — the v3 protocol, its methods, models, errors, jobs, and schemas.
- [Releasing](releasing.md) — ad-hoc-signed GitHub releases, checksums, Gatekeeper, and local packaging.

## Providers

- [Codex](codex.md)
- [Claude](claude.md)
- [Cursor](cursor.md)
- [OpenCode Go](opencode.md)
- [Grok](grok.md)
- [Multiple accounts](multi-account.md)

These pages cover behavior you can see — in the app, on the wire, or on disk. Implementation notes that only matter to the code live next to the code.
