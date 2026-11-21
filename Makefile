# Kit helpers. Run from the kit (or repo) root.
BIN ?= $(CURDIR)/build/shopping
SEEDS ?= 10
EVENTS ?= 20000
STARTER ?= vendor/starter-duckdb
DEV ?= vendor/dev
ANSWER ?= vendor/dev-answer
OUT ?= out

.PHONY: test build build-rust test-rust test-bin test-bin-strict diff check clean fixtures dev check-dev

test:            ## reference edge cases
	python3 -m unittest discover -s tests -v

build:           ## native build of Team 67's C++ pipeline (cpp/pipeline, MIT) via build.sh
	sh build.sh

build-rust:      ## retained Rust pipeline in fast/ (not shipped)
	cd fast && cargo build --locked --release

test-rust:       ## parser differential, key encoding and forced collision tests
	cd fast && cargo test --locked --release

test-bin: build  ## same edge cases against the binary (kit-only checks skipped, see tests/test_reference.py)
	SS_SKIP_KIT_ONLY=1 SS_BIN=$(BIN) python3 -m unittest discover -s tests

test-bin-strict: build  ## as test-bin but also runs the kit-only checks beyond rules.md
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
