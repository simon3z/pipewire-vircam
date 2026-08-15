CC ?= cc
CFLAGS ?= -O2 -Wall -Wextra

PW_CFLAGS := $(shell pkg-config --cflags libpipewire-0.3)
PW_LIBS   := $(shell pkg-config --libs libpipewire-0.3)

# Primary targets.
#   redcam      : the Rust virtual camera (built by cargo into target/release)
#   redcam-test : the independent C oracle (consumer + red/size/fps verifier)
all: redcam redcam-test

# --- Rust producer (primary) ------------------------------------------------
redcam:
	cargo build --release

# --- cargo hygiene (format + lint) -----------------------------------------
fmt:
	cargo fmt --check
clippy:
	cargo clippy --all-targets -- -D warnings
package:
	cargo package

# --- C producer (reference implementation, kept for comparison) ------------
# Sources live in reference/ (standalone C, not part of the crate);
# the binaries are built to the repo root.
redcam-c: reference/redcam.c
	$(CC) $(CFLAGS) $(PW_CFLAGS) -o $@ $< $(PW_LIBS)

# --- C oracle (independent consumer + verifier) -----------------------------
redcam-test: reference/redcam-test.c
	$(CC) $(CFLAGS) $(PW_CFLAGS) -o $@ $< $(PW_LIBS)

test:
	./ci.sh

e2e:
	./e2e.sh

clean:
	rm -f redcam-c redcam-test
	cargo clean

.PHONY: all redcam fmt clippy package test bench clean
