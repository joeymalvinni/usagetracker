# Errors

API failures look like this:

```json
{"api_version":3,"type":"error","error":{"code":"unknown_account","message":"unknown account: …","retryable":false}}
```

`message` is for humans and may change, so branch on `code`. No API error carries a retry delay — use bounded client-side backoff. And for mutations, only retry when you know the operation is idempotent or you've read its state back.

| Code | Retryable? | What it means / what to do |
| --- | --- | --- |
| `invalid_json` | no | The frame isn't valid UTF-8 JSON. Fix it. |
| `invalid_request` | no | Wrong envelope or parameters for a known method. Check the request schema. |
| `request_too_large` | no | The frame is over 64 KiB. Shrink it — the connection closes. |
| `unsupported_method` | no | The method isn't implemented in v3. |
| `incompatible_protocol` | no | `api_version` is missing or isn't v3. Rebuild and upgrade together. |
| `invalid_argument` | no | A value or config/setup update is semantically invalid. Correct it. |
| `unknown_provider` | no | That provider ID isn't supported. |
| `unknown_account` | no | The account doesn't exist or was deleted. Refresh your account state. |
| `unknown_refresh_job` | no | The job ID is invalid, was evicted, or was lost on restart. Start a new refresh if you still need one. |
| `unsupported_operation` | no | The provider or account doesn't have that capability. Check `get_server_info`. |
| `conflict` | no | Reserved in v3; the current daemon never emits it. |
| `storage_unavailable` | yes | SQLite or read state failed. Retry with backoff; if it persists, you're looking at local recovery. |
| `internal` | yes | An unexpected action or serialization failure. Read state before retrying a mutation. |

## Provider failures

Provider collection failures aren't API errors. A `refresh` usually produces a `completed` job that *contains* failed `provider_results` — see [models](models.md#health-and-refresh-results). Rate-limited accounts back off using the provider's own retry info when it's available, otherwise 5, 10, 20, 40, then at most 60 minutes. That deadline isn't exposed in v3.

When the connection limit is exhausted or a write fails, the connection just closes with no error envelope. Reconnect with bounded backoff.
