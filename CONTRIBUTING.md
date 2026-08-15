# Contributing

Thanks for helping improve Embrasure.

1. Open an issue before a large behavior or report-schema change.
2. Keep Snowflake credentials in environment variables or local key files. Never add credentials, tokens, profiles, or query results to fixtures.
3. Add deterministic tests for new selection, comparison, lineage, or output behavior.
4. Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --locked`.

Pull requests must preserve the stable exit-code contract and must not weaken CI-schema cleanup safeguards.
