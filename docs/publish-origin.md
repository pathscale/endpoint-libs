# Publish workflow origin gate

The `publish` job runs only for a push to `master` or a `workflow_dispatch`
whose ref is `master`. Its `crates-io` environment must be configured to allow
deployments from `master` only and must hold `CARGO_REGISTRY_TOKEN` before
publishing is activated.

Repository workflow YAML cannot guarantee those environment rules against a
workflow edited on another branch. Confirm the environment's deployment branch
restriction and secret configuration independently before treating the publish
gate as closed.
