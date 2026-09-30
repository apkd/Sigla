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

test_command=(cargo test --locked)
if [[ ${SIGLA_TEST_WITH_SUDO:-0} == 1 ]]; then
    # Sandboxed tests drop capabilities, so their .NET home must belong to root.
    test_dotnet_home=$(sudo mktemp -d /tmp/sigla-ci-dotnet.XXXXXXXX)
    trap 'sudo rm -r -- "$test_dotnet_home"' EXIT
    test_command=(sudo env "PATH=$PATH" "HOME=$test_dotnet_home" "DOTNET_CLI_HOME=$test_dotnet_home"
        "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}"
        "CARGO_PROFILE_DEV_DEBUG=$CARGO_PROFILE_DEV_DEBUG" "CARGO_PROFILE_TEST_DEBUG=$CARGO_PROFILE_TEST_DEBUG"
        "${test_command[@]}")
fi

"${test_command[@]}" --all-targets
# CI provides the fixtures and SDK for these ignored tests. Other ignored tests
# require manual tools or external data and must be requested separately.
"${test_command[@]}" --test metadata -- --ignored
"${test_command[@]}" --lib csharp::oracle:: -- --ignored
