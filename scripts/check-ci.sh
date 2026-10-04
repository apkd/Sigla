#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0

phase=${1:-all}
case "$phase" in
    all|lint|test) ;;
    *) echo "Usage: $0 [all|lint|test]" >&2; exit 2 ;;
esac

if [[ "$phase" != test ]]; then
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
fi
if [[ "$phase" == lint ]]; then exit 0; fi

dotnet build tests/metadata-fixture -c Release
dotnet build tests/metadata-oracle -c Release
mkdir -p target
dotnet tests/metadata-oracle/bin/Release/net10.0/MetadataOracle.dll --signatures tests/metadata-fixture/bin/Release/net10.0/MetadataFixture.dll > target/signatures.json
cargo run --locked --example metadata_compare -- tests/metadata-fixture/bin/Release/net10.0/MetadataFixture.dll target/signatures.json

test_command=(cargo)
if [[ ${SIGLA_TEST_WITH_SUDO:-0} == 1 ]]; then
    # Sandboxed tests drop capabilities, so their .NET home must belong to root.
    test_dotnet_home=$(sudo mktemp -d /tmp/sigla-ci-dotnet.XXXXXXXX)
    trap 'sudo rm -r -- "$test_dotnet_home"' EXIT
    test_command=(sudo --preserve-env=CARGO_BUILD_TARGET,CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER,CC_x86_64_unknown_linux_musl,LIBARCHIVE_LIB_DIR,LIBARCHIVE_INCLUDE_DIR,LIBARCHIVE_STATIC,LIBARCHIVE_LDFLAGS
        env "PATH=$PATH" "HOME=$test_dotnet_home" "DOTNET_CLI_HOME=$test_dotnet_home"
        "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}"
        "CARGO_PROFILE_DEV_DEBUG=$CARGO_PROFILE_DEV_DEBUG" "CARGO_PROFILE_TEST_DEBUG=$CARGO_PROFILE_TEST_DEBUG"
        "RUSTFLAGS=${RUSTFLAGS:-}" "CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-1}"
        "${test_command[@]}")
fi

if [[ ${SIGLA_TEST_REPORTS:-0} == 1 ]]; then
    # Separate reports prevent the SDK-only runs from overwriting the main suite.
    status=0
    "${test_command[@]}" nextest run --locked --profile ci --lib --bins --test integration --test licenses || status=1
    "${test_command[@]}" nextest run --locked --profile metadata --test integration metadata:: --run-ignored only || status=1
    "${test_command[@]}" nextest run --locked --profile oracle --lib csharp::oracle:: --run-ignored only || status=1
    # The remote executable doubles as an SSH fixture and uses a custom harness.
    remote_result=Passed
    "${test_command[@]}" test --locked --test remote || { status=1; remote_result=Failed; }
    if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
        printf '\n### Remote lifecycle checks\n\n| Result |\n| --- |\n| %s |\n' "$remote_result" >> "$GITHUB_STEP_SUMMARY"
    fi
    exit "$status"
fi

# Examples are checked by Clippy; they contain no tests and need no test executables.
"${test_command[@]}" test --locked --lib --tests
# CI provides the fixtures and SDK for these ignored tests. Other ignored tests
# require manual tools or external data and must be requested separately.
"${test_command[@]}" test --locked --test integration metadata:: -- --ignored
"${test_command[@]}" test --locked --lib csharp::oracle:: -- --ignored
