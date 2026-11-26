# Contributing

1. Preserve the strict lower-triangular module dependency order verified by `tests/mcp_integration.rs`.
2. Keep all tests deterministic and zero-billing—never spawn real `colab`, `ssh`, `git`, or `curl` processes inside tests.
3. Keep the packed panel (`panel/dist/panel.html`) zero-`innerHTML`, dark-first, and `<= 43,500 B` (hard cap `< 46,080 B`).
4. Run all checks before opening a PR:
   ```sh
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test --locked
   node panel/pack.mjs && node --test panel/test/panel.test.mjs
   bash scripts/check_sanitized.sh
   ```
5. Use Conventional Commits (`feat:`, `fix:`, `perf:`, `docs:`, `refactor:`, `test:`, `chore:`).
