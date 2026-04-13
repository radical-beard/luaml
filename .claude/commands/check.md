Quick pre-commit check — validates code without running the full test suite:

1. `cargo fmt --all -- --check` — verify formatting
2. `cargo clippy --workspace` — lint
3. `cargo test --no-run` — compile tests without running them

If all pass, report "Ready to commit."
If any fail, fix the issues and re-check.
