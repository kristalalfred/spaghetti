default:
    @just --list

build:
    cargo build --release --workspace

check:
    cargo clippy --workspace --all-targets
    cargo test --workspace
    cargo test -p spaghetti-engine --lib -- --ignored ring_model

check-ios:
    IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo clippy -p spaghetti-engine --all-targets --target aarch64-apple-ios -- -D warnings
    IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo clippy -p spaghetti-engine --all-targets --target aarch64-apple-ios-sim -- -D warnings

install:
    ./scripts/install.sh

run *args:
    cargo run -p spaghetti -- {{args}}

logs:
    tail -f ~/Library/Logs/spaghetti/spaghettid.log /tmp/spaghetti-driver.log
