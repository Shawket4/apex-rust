.PHONY: build run test clean release docker help

help:
    @echo "Available commands:"
    @echo "  make build    - Build the project in debug mode"
    @echo "  make release  - Build the project in release mode"
    @echo "  make run      - Run the project in debug mode"
    @echo "  make test     - Run tests"
    @echo "  make clean    - Clean build artifacts"
    @echo "  make docker   - Build Docker image"
    @echo "  make watch    - Run with auto-reload"

build:
    cargo build

release:
    cargo build --release

run:
    cargo run

test:
    cargo test

clean:
    cargo clean

docker:
    docker build -t apex:latest .

watch:
    cargo watch -x run

lint:
    cargo clippy -- -D warnings
    cargo fmt --check

format:
    cargo fmt

bench:
    cargo bench
