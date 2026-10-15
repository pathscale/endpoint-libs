# Config logging policy

`load_config` does not log loaded configuration values. Consumer configurations can
contain credentials, database URLs, authentication headers, and signing material, so
printing their `Debug` representation can expose sensitive values in process logs.

The `Debug` bound on `load_config` remains for API compatibility. Loading a
configuration must not invoke `Debug`, `Display`, or serialization on the loaded
value for logging.

Config file selection, entry selection, merge precedence, and parsing errors are
unchanged.

Run `cargo run --example config-log-acceptance` for a subprocess witness that
checks empty stdout/stderr, merged values, and a Debug implementation that panics
if invoked.
