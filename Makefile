# Kit helpers. Run from the kit (or repo) root.
BIN ?= $(CURDIR)/build/launch
# The launcher picks Team 67's binary for canonical layouts and the Rust pipeline otherwise.
export SS_FAST_BIN ?= $(CURDIR)/build/shopping
export SS_GENERAL_BIN ?= $(CURDIR)/fast/target/release/shopping_sessions
# Native tests exercise Team 67's binary on any host; the image checks the CPU.
export SS_FORCE_FAST ?= 1
SEEDS ?= 10
EVENTS ?= 20000
STARTER ?= vendor/starter-duckdb
DEV ?= vendor/dev
ANSWER ?= vendor/dev-answer
OUT ?= out

.PHONY: test build build-rust test-rust test-bin test-bin-strict diff check clean fixtures dev check-dev

test:            ## reference edge cases
	python3 -m unittest discover -s tests -v

build:           ## Team 67's C++ pipeline (cpp/pipeline, MIT), the layout launcher, and the Rust general path
	sh build.sh
	cc -O2 -o build/launch cpp/launcher/launch.c
	cd fast && cargo build --locked --release

build-rust:      ## Rust general pipeline in fast/ only
	cd fast && cargo build --locked --release

test-rust:       ## parser differential, key encoding and forced collision tests
	cd fast && cargo test --locked --release

test-bin: build  ## same edge cases through the launcher (two Rust-writer layout checks skipped)
	SS_SKIP_KIT_ONLY=1 SS_BIN=$(BIN) python3 -m unittest discover -s tests

test-bin-strict: build  ## as test-bin, also asserting the Rust writer's file and row-group layout
	SS_BIN=$(BIN) python3 -m unittest discover -s tests

diff: build      ## reference vs binary on kit-generated dirty data
	bash tools/diff_impls.sh $(BIN) $(SEEDS) $(EVENTS)

check:           ## self-check an output dir: make check OUT=/tmp/out
	python3 tools/compare_outputs.py --check $(OUT)

fixtures: build  ## run both implementations against the official starter fixtures
	python3 $(STARTER)/scripts/run_fixtures.py $(STARTER)/fixtures -- python3 reference/pipeline.py
	python3 $(STARTER)/scripts/run_fixtures.py $(STARTER)/fixtures -- $(BIN)

dev:             ## generate exactly the public dev set
	cargo build --locked --release --manifest-path vendor/generator/Cargo.toml
	vendor/generator/target/release/generate --seed 1 --events 1000000 --clicks 250000 --output $(DEV)

check-dev:       ## check both tables with both the official and kit checkers
	python3 $(STARTER)/scripts/compare.py $(OUT) $(ANSWER)
	python3 tools/compare_outputs.py $(ANSWER) $(OUT)

clean:
	rm -rf build
	cd fast && cargo clean
