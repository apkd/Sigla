//! Synthetic sources shared by the local and remote coverage regressions.
pub const FILES: &[(&str, &str)] = &[
    (
        "scripts/plugin.lua",
        concat!(
            "-- Synthetic fixture.\r\n",
            "local function coverage_lua() return \"λ CoverageMarker\" end\r\n",
            "return coverage_lua\r\n",
        ),
    ),
    (
        "scripts/helper.py",
        "# Synthetic fixture.\ndef coverage_python():\n    return \"CoverageMarker\"\n",
    ),
    (
        "queries/filter.jq",
        ".items[] | select(.enabled) | .name # CoverageMarker\n",
    ),
    (
        "scripts/probe.bt",
        "BEGIN { printf(\"CoverageMarker\\n\"); }\n",
    ),
    (
        "tests/example_test.go",
        "package example\n\nfunc example() string { return \"CoverageMarker\" }\n",
    ),
    (
        "config/metrics.alloy",
        "logging { level = \"info\" } // CoverageMarker\n",
    ),
    (
        "config/common.rules",
        "prefix_rule(pattern = [\"CoverageMarker\"], decision = \"allow\")\n",
    ),
    (
        "config/storage.rules.in",
        "prefix_rule(pattern = [\"@CoverageMarker@\"], decision = \"allow\")\n",
    ),
    (
        "assets/theme.tmTheme",
        "<plist><dict><key>name</key><string>CoverageMarker</string></dict></plist>\n",
    ),
    (
        "assets/icon.svg",
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><title>CoverageMarker</title></svg>\n",
    ),
    (
        "queries/metrics.logql",
        "{service=\"fixture\"} |= \"CoverageMarker\"\n",
    ),
    (
        "patches/change.patch",
        "--- a/example.txt\n+++ b/example.txt\n@@ -1 +1 @@\n-before\n+CoverageMarker\n",
    ),
    ("config/pins.json", "{\n  \"pin\": \"CoverageMarker\"\n}\n"),
    (
        ".agents/skills/review/SKILL.md",
        "# Synthetic instructions\n\nCoverageMarker: inspect tests.\n",
    ),
    (
        "apple/Sources/Example.swift",
        "let message = \"CoverageMarker\"\n",
    ),
    (
        "apple/Modules/Example.swiftinterface",
        "public let message: Swift.String = \"CoverageMarker\"\n",
    ),
    (
        "apple/Sources/Example.m",
        "NSString *message = @\"CoverageMarker\";\n",
    ),
    (
        "apple/Sources/Example.mm",
        "const char *message = \"CoverageMarker\";\n",
    ),
    (
        "apple/Shaders/Example.metal",
        "constant char message[] = \"CoverageMarker\";\n",
    ),
    (
        "apple/Example.xcodeproj/project.pbxproj",
        "// !$*UTF8*$!\n{ message = CoverageMarker; }\n",
    ),
    (
        "apple/Info.plist",
        "<plist><dict><key>name</key><string>CoverageMarker</string></dict></plist>\n",
    ),
    (
        "apple/Config/Build.xcconfig",
        "PRODUCT_NAME = CoverageMarker\n",
    ),
    (
        "apple/Config/App.entitlements",
        "<plist><dict><key>CoverageMarker</key><true/></dict></plist>\n",
    ),
    (
        "apple/en.lproj/Localizable.strings",
        "\"message\" = \"CoverageMarker\";\n",
    ),
    (
        "apple/en.lproj/Localizable.stringsdict",
        "<plist><dict><key>CoverageMarker</key><dict/></dict></plist>\n",
    ),
    (
        "apple/Localizable.xcstrings",
        "{\"strings\":{\"CoverageMarker\":{}}}\n",
    ),
    (
        "apple/Views/Main.storyboard",
        "<document><view id=\"CoverageMarker\"/></document>\n",
    ),
    (
        "apple/Views/Main.xib",
        "<document><view id=\"CoverageMarker\"/></document>\n",
    ),
    (
        "apple/PrivacyInfo.xcprivacy",
        "<plist><dict><key>CoverageMarker</key><true/></dict></plist>\n",
    ),
    (
        "apple/Example.xcodeproj/xcshareddata/xcschemes/Example.xcscheme",
        "<Scheme name=\"CoverageMarker\"/>\n",
    ),
    (
        "apple/Example.xcworkspace/xcshareddata/WorkspaceSettings.xcsettings",
        "<plist><dict><key>CoverageMarker</key><true/></dict></plist>\n",
    ),
    (
        "apple/Example.xcworkspace/contents.xcworkspacedata",
        "<Workspace location=\"group:CoverageMarker\"/>\n",
    ),
    (
        "apple/Example.xctestplan",
        "{\"name\":\"CoverageMarker\"}\n",
    ),
    (
        "apple/Modules/module.modulemap",
        "module CoverageMarker { header \"example.h\" }\n",
    ),
    (
        "apple/Package.resolved",
        "{\"originHash\":\"CoverageMarker\",\"pins\":[]}\n",
    ),
    (
        "web/app.ts",
        "export const marker: string = 'CoverageMarker';\n",
    ),
    (
        "web/App.vue",
        "<template><p>CoverageMarker</p></template>\n",
    ),
    (
        "web/example.JS.MAP",
        "{\"version\":3,\"sources\":[\"CoverageMarker\"]}\n",
    ),
    ("jvm/Example.kt", "val message = \"CoverageMarker\"\n"),
    ("scripts/example.rb", "message = 'CoverageMarker'\n"),
    (
        "systems/example.zig",
        "const message = \"CoverageMarker\";\n",
    ),
    ("systems/example.res", "let message = \"CoverageMarker\"\n"),
    (
        "schemas/example.proto",
        "syntax = \"proto3\";\nmessage CoverageMarker {}\n",
    ),
    ("queries/example.sql", "SELECT 'CoverageMarker';\n"),
    ("infra/example.tf", "variable \"CoverageMarker\" {}\n"),
    ("build/rules.bzl", "marker = \"CoverageMarker\"\n"),
    ("docs/example.mdx", "# CoverageMarker\n\nAn example.\n"),
    ("data/example.csv", "name,value\nexample,CoverageMarker\n"),
    (
        "data/example.ipynb",
        "{\"cells\":[{\"source\":[\"CoverageMarker\"]}],\"nbformat\":4}\n",
    ),
    (
        "go/go.mod",
        "module example.invalid/CoverageMarker\n\ngo 1.25\n",
    ),
    ("config/.env.example", "MESSAGE=CoverageMarker\n"),
    ("config/.clang-format", "BasedOnStyle: CoverageMarker\n"),
];
