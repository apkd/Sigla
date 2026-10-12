# Pinned history inputs

These are complete, unchanged source files downloaded from the public repositories below. Tests put the snapshots in small constructed Git graphs to isolate each relationship. Their generated commit IDs are not the original repository IDs. The remote checks use the original IDs independently.

| Files | Repository | Child | Parent(s) | Original path |
|---|---|---|---|---|
| `checkstyle-rename-*` | https://github.com/checkstyle/checkstyle | `0e3fe5643667a53079dbd114e5b1e9aa91fde083` | `f34eba11cbd8559d4d06f96e19affa70abaf86ff` | `src/checkstyle/com/puppycrawl/tools/checkstyle/Checker.java` |
| `checkstyle-wrapper-*` | https://github.com/checkstyle/checkstyle | `f2c6263e151e8a7300755b36fbb41511c0a0cca1` | `2ba55edc4e35866afbb154e3ea03cf03e56ab9bc` | `src/main/java/com/puppycrawl/tools/checkstyle/utils/CommonUtils.java` |
| `lang-wrapper-*` | https://github.com/apache/commons-lang | `c503d742f094dc2048b72c3f78f5e2e6070a44e1` | `1a1fc65b04bcbe2e95d0fa300fce110e1224906e` | `src/main/java/org/apache/commons/lang3/math/NumberUtils.java` |
| `junit-*` | https://github.com/junit-team/junit4 | `a49240ade1974b948b20cf2c45d9129f04122735` | `708ed373c02b467422890d5163fff46d1f32ab01`, `9d8bb069f68e2194db742981972c8930381b62c2` | `src/main/java/org/junit/runners/BlockJUnit4ClassRunner.java` |
| `rust-*` | https://github.com/rust-lang/rust | `7c360dc117d554a11f7193505da0835c4b890c6f` | `392d2728683f140f6125732240e462c43c5caff4` | `library/core/src/lazy.rs`, `library/core/src/cell/lazy.rs`, `library/core/src/cell/once.rs` |
| `csharp-guid-utf8-*` | https://github.com/dotnet/runtime | `50bf1685bdb685ef0304c7686e92e2da9f02b2de` | `1c6d238660cb70fbdb146220b89b08cb54d78ad8` | `src/libraries/System.Private.CoreLib/src/System/Guid.cs` |
| `csharp-int32-interface-*` | https://github.com/dotnet/runtime | `7f2cdc538d12bf92c8ef3811e8270a02ecd712e8` | `a1bc0f34fc8ad77c31a1682841d92dbb20e39dd8` | `src/libraries/System.Private.CoreLib/src/System/Int32.cs` |
| `csharp-buffer-*`, `csharp-bytememops-after.cs` | https://github.com/dotnet/runtime | `fab69efde7d2458dfb23e01b686842cf6d7f576d` | `79dd9bae9bb881eb716b608577c4cedc6c9cba72` | `src/libraries/System.Private.CoreLib/src/System/Buffer.cs`, `src/libraries/System.Private.CoreLib/src/System/SpanHelpers.ByteMemOps.cs` (added) |

Original license and copyright notices remain in the files. The corresponding repository license texts and Commons Lang notice are copied under `licenses/`: GPL2-or-later for the older Checkstyle pair, LGPL2.1-or-later for the overload pair, EPL1 for JUnit, Apache2 for Commons Lang, and MIT or Apache2 for Rust. These files are test inputs; they do not enter the released Sigla binary.

The C# snapshots retain the .NET Foundation's MIT notices; the repository license is `licenses/runtime-license.txt`.
