# Changelog

All notable changes to this project will be documented in this file.
## [1.7.11] - 2026-04-28

### Features

- Add wtx backend

## [1.7.6] - 2026-04-27

### Refactor

- Abstract server logic away from underlying lib backend implementation

## [1.7.0] - 2026-04-25

### Bug Fixes

- Fix panic during H2 handler init

### Features

- Make h2 + TLSv1.3 the default, with anything lower explicit opt-in

## [1.6.0] - 2026-04-21

### Miscellaneous Tasks

- Remove OtelProtocol concept + grpc & tonic since we are only using http

## [1.5.1] - 2026-04-13

### Bug Fixes

- Fix rustls deps to explicitly use ring, to remove implicit aws-lc-sys dep

### Features

- Add better logging when user inits logging outside of tokio runtime

### Miscellaneous Tasks

- Add some more otel layer debug logging
- Add some more logging

## [1.5.0] - 2026-04-08

### Features

- Implement otel log forwarding feature

## [1.4.1] - 2026-04-03

### Miscellaneous Tasks

- Update deps due to yanked dep warning

## [1.4.0] - 2026-04-03

### Features

- WsClient: Return initial response in connect request, and add send_raw fn for pre-serialized message

## [1.3.7] - 2026-03-31

### Bug Fixes

- WS server: Check for unimplemented method ID earlier

## [1.3.6] - 2026-03-31

### Bug Fixes

- Handle lack of protocol header gracefully in ws client

### Features

- Add a bunch of logging into the ws-echo example

### Miscellaneous Tasks

- Publish ws-echo example image
- Add some ws CLI examples

### Example

- Expand ws-echo example to support HoneyReceiveUserInfo simulated endpoint

## [1.3.5] - 2026-03-28

### Miscellaneous Tasks

- Add --no-tag option to release script
- Migrate to ubicloud build machine

## [1.3.4] - 2026-03-06

### Miscellaneous Tasks

- Update deps.rs badge to v1.3.1
- Convert release script to use cargo-release and git-cliff

## [1.3.1] - 2026-03-06

### Bug Fixes

- Add Cargo.lock file to git

### Miscellaneous Tasks

- Update deps.rs badge to v1.3.1

## [1.3.0] - 2026-03-06

### Bug Fixes

- Changed user id to u64
- Made lifetimes explicit (#16)
- Removed needless unwrap (#20)
- Fix tests and make config struct fields pub
- Variable name
- Use stable Duration::from_secs instead of from_mins
- Don't serialize the EnumRef::prefixed_name field, avoids including it in FE-facing services.json

### Features

- Switch to buildjet (#3)
- Added well-known error codes as consts (#14)
- Replaced alloy by alloy_primitives (#15)
- Added description to fields (#17)
- Add warning when running server in insecure mode
- Feature gating and dependency cleanup
- Feature gating and dependency cleanup
- Custom logger setup
- Initial implementation of error aggregation logging feature
- Add runtime log level reloading, major architecture refactor, testing
- Improve CI script
- Implement log throttling feature & websocket caller tracking
- Allow client code to shutdown rate limit layer gracefully by returning a handle to it
- Types changes for endpointgen improvements

### Miscellaneous Tasks

- Improve error propagation in private key loading function slightly, original error was being lost
- Some minor module path exporting changes to minimize required changes in client code
- Add comment for pub type re-export
- Add todo for error aggregation feature
- Logging code cleanup and reorganize after error aggregation addition
- Clippy suggestions
- More clippy suggestions
- Fix tests, ignore codeblocks in doc comments
- Formatting
- Add README, TODO, release script, and CI improvements
- Formatting and deprecation warning fix
- Fix deps.rs badge and add badge update to release script

### Performance

- Removed needless Arc::clone on role check (#13)

### Refactor

- Clippy and formatting (#8)

### Hack

- Update field description to be skipped via serde

## [1.0.0] - 2024-10-07


