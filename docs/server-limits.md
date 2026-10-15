# WebSocket server limits

Set an individual option to `None` to opt out. Omitted configuration fields use
the same defaults as `Default`. Durations use Serde's seconds/nanoseconds form.

| Option | Default | Coverage |
| --- | ---: | --- |
| `max_connections` | 1024 | Accepted transports before TLS, through session teardown |
| `max_message_bytes` | 1 MiB | WebSocket messages |
| `max_inflight_requests` | 32 | Legacy and MCP handlers across the server and all shards |
| `request_timeout` | 30 seconds | Before hooks, handler and after hooks together |
| `handshake_timeout` | 10 seconds | One absolute budget for TLS and HTTP upgrade; auth is separately bounded by the same duration |
| `idle_timeout` | 120 seconds | Time since a successfully received frame |

Admission happens before handler task allocation. Busy MCP calls receive -32000;
expired requests receive -32001 with the original id. Notifications are ignored without allocating handler work or producing a
response. After hooks share the request deadline; expiry after a handler has
already answered cancels remaining hook work without sending a second response.
Outbound traffic does not extend the idle deadline, including blocked sends.
Accept failures back off from 10 milliseconds to one second while live sessions
and shutdown continue to be polled. A successful accept resets the backoff.

Registered connection state is removed synchronously by an ownership guard,
including auth failure, session cancellation and shutdown. Disconnect hooks are
dispatched exactly once to an independent Nagoya task with its own reactor;
the dispatcher awaits the hooks, and connection teardown detaches the task. Hooks must be valid
on a Nagoya worker and cannot depend on the original connection's reactor or
thread-local state. Hook completion requires a running process; process exit,
panic or reactor creation failure cannot guarantee asynchronous completion.
Auth errors are flushed for at most one second before the transport is dropped.
