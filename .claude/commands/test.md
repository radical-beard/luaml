Run the complete luaml test and lint suite:

1. Run `cargo fmt --all -- --check` to verify formatting
2. Run `cargo clippy --workspace` to check for lint issues
3. Run `cargo test` to run all tests

If any step fails:
- For fmt: run `cargo fmt --all` to fix, then show what changed
- For clippy: show the warnings/errors and suggest fixes
- For tests: show the failing test names and investigate the root cause

Report a summary: formatting status, clippy warnings, total tests run, pass/fail count.
