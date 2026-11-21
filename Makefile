# Kit helpers. Run from the kit (or repo) root.
BIN ?= $(CURDIR)/fast/target/release/shopping_sessions
SEEDS ?= 10
EVENTS ?= 20000

.PHONY: test build test-bin diff check clean

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

clean:
	cd fast && cargo clean
