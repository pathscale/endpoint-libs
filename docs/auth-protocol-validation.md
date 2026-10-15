# WebSocket auth protocol validation

`EndpointAuthController` parses comma-separated `Sec-WebSocket-Protocol` segments.
Each segment is trimmed, and empty segments are skipped. A segment is accepted only
when its first byte is an ASCII decimal digit (`0` through `9`). Its first byte is
the borrowed discriminator key; the remaining text is the borrowed payload.

Segments with non-ASCII or otherwise invalid discriminators are ignored. Duplicate
discriminators retain the existing `HashMap` behavior: the last value wins.
Whitespace around segments is trimmed, while whitespace within a payload is kept.
The method discriminator is `0`; if it is missing or does not identify a registered
endpoint, authentication returns an error before creating an authenticated request
context.

## Integration acceptance

Use the admission lane's transport acceptance fixture as an external integrator
witness. Send real WebSocket upgrades through `EndpointAuthController` with
`Sec-WebSocket-Protocol: éx`, multibyte-only tokens, an empty list, and a valid
known fixture method. Malformed cases must be refused without panic or partial valid
identity. Confirm the shard handles a subsequent valid connection and that the valid
fixture authenticates. Exercise a nonzero shard as well.

The witness should fail if the original byte slicing (`&x[..1]`, `&x[1..]`) is
restored. Run `check-features` and `check-chain` through the integrator before release.
