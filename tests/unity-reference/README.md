# Unity reference fixtures

Test-only Linux Unity 6000.3.10f1 and 2022.3.62f3 captures: compilation
inputs/graphs, `modules.asset`, and DLL inventories. `<EDITOR_DATA>` means
`Editor/Data`. Tests use empty DLL placeholders. No Unity or network required.
Normal discovery must never run the exporter or start Unity.

Baseline and `advanced/` cover both API profiles and assembly rules. Plugins,
package testability, and version-specific UI references remain untested.

Regenerate with the matching editor:

1. Create baseline and advanced fixtures:
   `cargo run --example unity_fixture -- DIRECTORY VERSION CONDUIT_PACKAGE [advanced]`.
   For 2022.3, disable the bootstrap bridge's Editor assembly (requires Unity 6).
2. Add `ExportCompilation.cs` to `Assets/Editor`. Select Linux standalone;
   disable development builds.
3. Write `{"directory":"/absolute/output/directory","stage":0}` to the project's
   `capture-request.json` and wait. Capture removes the bootstrap package and
   request, exports both API profiles, and leaves .NET Framework selected.
   Use `PlayerWithoutTestAssemblies`; `Player` includes test symbols.
4. Package baseline and `advanced/` captures:
   `cargo run --example unity_reference -- CAPTURE_DIRECTORY /path/to/Editor/Data tests/unity-reference/VERSION.tar.zst`.

Inspect: `tar --zstd -xf tests/unity-reference/VERSION.tar.zst -C DIRECTORY`.
