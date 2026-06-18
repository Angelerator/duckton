.PHONY: clean clean_all all configure debug release test test_debug test_release

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# The loadable extension name (lower case). Produced artifact:
#   build/release/duckton.duckdb_extension
EXTENSION_NAME=duckton

# duckdb-rs relies on the UNSTABLE C extension API, so the produced binaries are
# pinned to the exact TARGET_DUCKDB_VERSION (forwards compatibility is broken).
USE_UNSTABLE_C_API=1

# Target DuckDB version. MUST match the `duckdb` crate pinned in
# crates/extension/Cargo.toml (duckdb-rs 1.10504.0 <-> DuckDB v1.5.4).
TARGET_DUCKDB_VERSION=v1.5.4

all: configure debug

# Reusable makefiles from the pinned extension-ci-tools submodule. These drive
# the cargo build (rust.Makefile sets DUCKDB_EXTENSION_NAME/min-version env and
# copies the cdylib) and the metadata-footer append (base.Makefile). A bare
# `cargo build` at the repo root only builds `crates/extension` because the
# workspace sets `default-members = ["crates/extension"]`.
include extension-ci-tools/makefiles/c_api_extensions/base.Makefile
include extension-ci-tools/makefiles/c_api_extensions/rust.Makefile

configure: venv platform extension_version

debug: build_extension_library_debug build_extension_with_metadata_debug
release: build_extension_library_release build_extension_with_metadata_release

test: test_debug
test_debug: test_extension_debug
test_release: test_extension_release

clean: clean_build clean_rust
clean_all: clean_configure clean
