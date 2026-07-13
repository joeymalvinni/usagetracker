# Socket API v3

UsageTracker exposes a local API over a Unix socket — newline-delimited JSON, no ceremony.

- [Protocol](protocol.md) — the transport, framing, limits, timeouts, and a working example.
- [Versioning](versioning.md) — exact-match compatibility and the capabilities on offer.
- [Methods](methods.md) — all 19 request methods.
- [Models](models.md) — identifiers, units, timestamps, ordering, and what the models mean.
- [Errors](errors.md) — API errors and provider refresh failures.
- [Refresh jobs](refresh-jobs.md) — background work, coalescing, retention, and what a restart does.
- Schemas: [request](schemas/v3/request.json) and [response](schemas/v3/response.json).

The schemas are generated straight from the Rust wire types, so they're the final word. `cargo test -p usage-core checked_in_protocol_schemas_are_current` catches any drift, and you can regenerate them with:

```sh
cargo run -p usage-core --example generate-schemas -- docs/api/schemas/v3
```

The CLI's `--style json` output is a separate interface — see the [CLI reference](../cli.md#json-output).
