# WebSocket server limits

Existing controls now have bounded defaults. Set an individual option to `None`
to opt out. Omitted configuration fields use the same defaults as `Default`.

| Option | Default | Coverage |
| --- | ---: | --- |
| `max_connections` | 1024 | WebSocket upgrade and connection lifetime, after TLS |
| `max_message_bytes` | 1 MiB | WebSocket messages |
| `max_inflight_requests` | 32 | Legacy handlers across the server and all shards |
| `request_timeout` | 30 seconds | Legacy handler only, excluding hooks |

The admission artifact was incomplete. MCP handler admission and deadlines,
whole-request deadlines including hooks, handshake and idle deadlines, generic
transport connection admission, auth-failure state cleanup, accept-error backoff,
and the requested real-transport acceptance fixture remain outstanding. No
handshake or idle configuration fields are exposed without enforcement.
