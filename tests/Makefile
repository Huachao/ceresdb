
MODE ?= debug
ROOT = $(shell pwd)
DATA_DIR = /tmp/ceresdb

export CERESDB_BINARY_PATH ?= $(ROOT)/../target/$(MODE)/ceresdb-server
export CERESDB_CONFIG_PATH ?= $(ROOT)/../docs/harness.toml
export CERESDB_SERVER_ENDPOINT ?= 127.0.0.1:8831
export CERESDB_TEST_CASE_PATH ?= $(ROOT)/cases
export CERESDB_TEST_HARNESS ?= $(ROOT)/../target/$(MODE)/ceresdb-test
export CERESDB_STDOUT_FILE ?= /tmp/ceresdb-stdout.log
export CERESDB_STDERR_FILE ?= /tmp/ceresdb-stderr.log

clean:
	rm -rf $(DATA_DIR)

build-ceresdb:
	cd .. && cargo build --bin ceresdb-server

build-harness:
	cd harness && cargo build

build: build-ceresdb build-harness

# rename ${case}.out to ${case}.result automatically. fd is required
# https://github.com/sharkdp/fd
fix:
	fd -t f out --exec mv {} {.}.result \;

run: clean build
	cd harness && $(CERESDB_TEST_HARNESS)
