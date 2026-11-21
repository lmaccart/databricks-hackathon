# Kit helpers. Run from the kit (or repo) root.
BIN ?= $(CURDIR)/fast/target/release/shopping_sessions
SEEDS ?= 10
EVENTS ?= 20000
STARTER ?= vendor/starter-duckdb
DEV ?= vendor/dev
ANSWER ?= vendor/dev-answer
OUT ?= out

.PHONY: test build test-bin diff check clean fixtures dev check-dev

test:            ## reference edge cases
	python3 -m unittest discover -s tests -v

build:           ## release build of the fast pipeline
	cd fast && cargo build --locked --release

test-bin: build  ## same edge cases against the binary
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
	cd fast && cargo clean
