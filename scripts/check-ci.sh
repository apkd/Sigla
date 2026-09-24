#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0

cargo fmt --check
dotnet build tests/metadata-fixture -c Release
cargo clippy --locked --all-targets -- -D warnings
dotnet build tests/metadata-oracle -c Release
dotnet tests/metadata-oracle/bin/Release/net10.0/MetadataOracle.dll --signatures tests/metadata-fixture/bin/Release/net10.0/MetadataFixture.dll > target/signatures.json
cargo run --locked --example metadata_compare -- tests/metadata-fixture/bin/Release/net10.0/MetadataFixture.dll target/signatures.json

if [[ ${SIGLA_TEST_WITH_SUDO:-0} == 1 ]]; then
    sudo env "PATH=$PATH" "HOME=$HOME" "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}" \
        "CARGO_PROFILE_DEV_DEBUG=$CARGO_PROFILE_DEV_DEBUG" "CARGO_PROFILE_TEST_DEBUG=$CARGO_PROFILE_TEST_DEBUG" \
        cargo test --locked --all-targets -- --include-ignored
else
    cargo test --locked --all-targets -- --include-ignored
fi
