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
];
