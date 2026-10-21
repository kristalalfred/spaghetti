default:
    @just --list

build:
    cargo build --release --workspace

check:
    cargo clippy --workspace --all-targets
    cargo test --workspace

install:
    ./scripts/install.sh

tui:
    cargo run -p spaghetti-tui

logs:
    tail -f ~/Library/Logs/spaghetti/spaghettid.log /tmp/spaghetti-driver.log
