# automixah — Rust-only terminal auto-DJ.

# List all recipes
[group('meta')]
@list:
    just --list

# Check the whole workspace (native only)
[group('build')]
check:
    cargo check --workspace --all-targets

# Run unit/integration tests (native)
[group('build')]
test:
    cargo test --workspace

# Lint (clippy, warnings as errors)
[group('build')]
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format
[group('build')]
fmt:
    cargo fmt --all

# Format check (CI)
[group('build')]
fmt-check:
    cargo fmt --all -- --check

# Build the CLI binary (debug)
[group('build')]
build:
    cargo build -p automixah-cli

# Run: mix two tracks into mix.wav
[group('build')]
mix out tracks:
    cargo run -p automixah-cli -- --out {{out}} {{tracks}}

# Commit all changes with a message
[group('meta')]
commit msg:
    git add -A
    git commit -m "{{msg}}"
