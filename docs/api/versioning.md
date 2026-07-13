# Versioning

Protocol versioning is exact-match. The daemon accepts `api_version: 3`, and every response carries `api_version: 3` right back.

| Client | Daemon v3 |
| --- | --- |
| v3 | Supported |
| Missing, v1, v2, or something newer | `incompatible_protocol` |

`get_server_info` isn't a way to negotiate around this — its request already has to be v3. What it does report is the features and provider operations available within that version.

## Capabilities

| Capability | What it means |
| --- | --- |
| `typed_errors` | Errors use stable codes and a `retryable` flag. |
| `usage_provenance` | Usage responses include normalized source/scope/quality data. |
| `refresh_jobs` | Refresh work is started and polled as a job. |
| `refresh_coalescing` | Overlapping in-flight refresh work may be shared. |
| `combined_state` | `get_state` returns the whole dashboard state in one response. |

Separately, each provider descriptor advertises `multiple_accounts`, `add_account`, `repair`, `launch_account`, and `workspace_setup`.

## What can change, and what can't

- Current Rust and Swift decoders ignore unknown object fields — your client should too.
- Missing fields are only safe where the schema marks them optional or a documented default exists.
- Tagged methods, response `type` values, and most enums are closed. Adding one can require a protocol bump unless every supported client is made tolerant first.
- Removing or renaming a field, changing its type or meaning, or making an optional field required all require a protocol bump.
- Provider-specific `diagnostics` keys are opaque and can change without a protocol bump.

Since only v3 exists, there's no deprecation window yet. The app bundles its own daemon, so upgrade the two together; a CLI you distribute on its own has to match.
