# Protocol

## Transport

The daemon listens on an `AF_UNIX` stream socket. By default that's `~/.usagetracker/usage.sock` (or `$USAGE_TRACKER_HOME/usage.sock`), and you can move it with `--socket-path` or `USAGE_TRACKER_SOCKET`.

The default parent directory is `0700` and the socket is `0600`. There's no authentication beyond filesystem access — any same-user process that can connect has the full API described in [Security](../security.md).

## Framing and encoding

Each frame is one UTF-8 JSON object followed by a line feed (`0x0a`). CRLF is fine, and blank lines are ignored. If the connection hits EOF right after a non-empty but unterminated frame, that frame is currently submitted anyway — so always send the LF to be safe.

Requests and responses are flat envelopes:

```jsonl
{"api_version":3,"method":"get_server_info"}
{"api_version":3,"type":"server_info","server":{"api_version":3,"capabilities":[],"providers":[]}}
```

Invalid UTF-8 or JSON comes back as `invalid_json`. A non-object, a missing or non-string method, or a structurally broken known method comes back as `invalid_request`.

## How connections behave

- A connection stays open across many frames.
- Requests on one connection are handled one at a time, in order.
- Pipelining works, and responses come back in request order — v3 has no request IDs.
- The server closes the connection after client EOF, 30 seconds without a complete request frame, an oversized request or response, or a write failure or timeout.
- Up to 64 connections are served at once. Anything beyond that is accepted and then closed without a reply.
- There are no subscription or event connections.

## Limits and timeouts

| Limit | Value |
| --- | --- |
| Request frame | 64 KiB, terminator included, while reading |
| Response frame | 8 MiB, terminator included |
| Client connections | 64 |
| Incomplete / idle request | 30 seconds |
| Daemon response write | 30 seconds |

A request over 64 KiB gets `request_too_large` and the connection closes. If a response would serialize past 8 MiB, the daemon substitutes an `internal` error and closes.

First-party clients use their own shorter, operation-specific budgets — roughly 3 seconds for reads, 5 for ordinary updates, 10 for account and login actions, and 20 for provider setup discovery. Those are client-side budgets, not promises the server makes. The actual provider work behind a refresh runs in a background job.

## Example

```sh
printf '%s\n' '{"api_version":3,"method":"get_server_info"}' \
  | nc -U ~/.usagetracker/usage.sock
```

From here, see [methods](methods.md), [models](models.md), and the generated [request](schemas/v3/request.json) and [response](schemas/v3/response.json) schemas.
