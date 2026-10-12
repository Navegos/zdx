# xtask development guide

Scope: maintainer-only workspace utilities (defaults/config/codebase generation).

## Where things are

- `src/main.rs`: xtask command entrypoint
- `src/codebase.rs`: `codebase.txt` generation (`just codebase`)

## Conventions

- Keep xtask focused on repository maintenance workflows.
- Avoid runtime dependencies from product crates unless required.

## Checks

- Follow the root verification rules. Scoped tests: `cargo test -p xtask --lib --bins --tests`.
- Run commands from repo root via `cargo xtask ...` or `just update-*` recipes
