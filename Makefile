# findery — everything you need to build, check and run the stack.
.DEFAULT_GOAL := help
.PHONY: help build test clippy fmt fmt-check check bench layers sizes up-model up-model-gpu run install up up-gpu up-qdrant down logs ps shell health image clean

## --- Rust ------------------------------------------------------------------

build:            ## Build every crate in release mode
	cargo build --workspace --release

test:             ## Run the whole test suite
	cargo test --workspace

clippy:           ## Lint with warnings as errors
	cargo clippy --workspace --all-targets -- -D warnings

fmt:              ## Format the workspace
	cargo fmt --all

fmt-check:        ## Fail if anything is unformatted
	cargo fmt --all -- --check

layers:           ## Verify no crate depends on a layer above it
	bash tools/watchdogs/check_layers.sh

sizes:            ## Verify no file grows past its budget (CONV-029)
	bash tools/watchdogs/check_file_size.sh

check: fmt-check clippy test layers sizes  ## Everything CI would run

bench:            ## Measure the brute-force vector scan (release only)
	cargo run -p findery-bench --release

## --- Docker ----------------------------------------------------------------

image:            ## Build the daemon image
	docker compose build

up-model:         ## Start ONLY the model; run the daemon natively (recommended)
	docker compose up -d tei

up-model-gpu:     ## Same, on the GPU
	docker compose --profile gpu up -d tei-gpu

run:              ## Run the daemon on this machine (needs `up-model` first)
	cargo run --release -p findery-server

install:          ## Install findery, the daemon and the MCP bridge into ~/.cargo/bin
	cargo install --path crates/cli --force
	cargo install --path crates/server --force
	cargo install --path crates/mcp --force

up:               ## Start the FULL stack in Docker (daemon included)
	docker compose up -d

up-gpu:           ## Start the CUDA stack
	docker compose --profile gpu up -d

up-qdrant:        ## Start the stack with a local Qdrant vector database
	FINDERY_STORE_BACKEND=qdrant docker compose --profile qdrant up -d

down:             ## Stop everything, keep the volumes
	docker compose --profile gpu --profile qdrant down

logs:             ## Follow the daemon logs
	docker compose logs -f findery

ps:               ## Show the stack
	docker compose ps

health:           ## Ask the daemon what it is serving
	curl -fsS http://127.0.0.1:$${FINDERY_PORT:-8077}/v1/health

shell:            ## A shell inside the daemon container
	docker compose exec findery bash

clean:            ## Remove build artefacts (the index volume is untouched)
	cargo clean

help:             ## List the targets
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	 | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
