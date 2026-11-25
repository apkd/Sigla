// These writers preserve the 0.1.51/52 blob IDs, LMDB records and repository headers.
const LAYOUT_0_1_52: &str = "7ea55abcbd7073418e58ad18045ca16cc0d54dff835a696bfa3e1f772deb067c";
const ANALYSIS_0_1_52: &str = "6154a2c53407f83a0aa39cfff38a00f27aebacbee3a386f5df678fb30a5c42ae";
const BUFFERED_PAGE_ENCODER: &str =
    "5448e5e53db30c775b90ba19c79a49ab65584e1774bf12739f8ba8157b29302b";
const COMPATIBLE_WRITERS: &[&str] = &[
    "343eb29dc689806e25e79e36027290201a7f1a3237a05ae536af946a00a7aede", // First binding
    "f795f98ce053000a88db10bf395ba01e258a2559f2cac94877708f42295d2085", // Index marker
    "0264b8a95348bba494dee3707697fae41d175722baf356da58ed85f6b1403e79", // Key buffers
    "cf1fafbe06938f4ca2dcce73c20e46ae68549b8337b83635cd9b008171db5dfe", // Page encoder
    "ae02f87f1e4278276a31a96aa814ddba25fd5d063f5dc1e0ff3872e987621c74", // Immutable selector reuse
    "9fa87f3a65c2a0985ef79c066f233f0ad8452c2d43ec162f2b9915a2f4bd6810", // Pending commit inventories
];

/// Use the released format ID only for an exact, reviewed source digest in its domain.
pub fn compatible<'a>(domain: &str, digest: &'a str) -> &'a str {
    match domain {
        "LAYOUT" if COMPATIBLE_WRITERS.contains(&digest) => LAYOUT_0_1_52,
        "ANALYSIS" if digest == BUFFERED_PAGE_ENCODER => ANALYSIS_0_1_52,
        _ => digest,
    }
}

/// Source inputs to analysis, workspace, assets, asset records, and cache layout.
pub fn domains(name: &str) -> [bool; 5] {
    // These standalone modules are compiled only in test builds. Keep this list
    // explicit: a production module may legitimately have "test" in its name.
    if matches!(
        name,
        "src/workspace_tests.rs"
            | "src/workspace_binding_probe.rs"
            | "src/store/storage_tests.rs"
            | "src/store/format_tests.rs"
            | "src/store/workspace_tests.rs"
            | "src/store/storage_benchmark.rs"
            | "src/store/shared_tests.rs"
            | "src/store/segment_tests.rs"
            | "src/store/publication_tests.rs"
            | "src/metadata_archive/benchmark.rs"
            | "src/metadata_archive/tests.rs"
            | "src/service/admission_tests.rs"
            | "src/csharp/cache_tests.rs"
            | "src/history/regions/smoke_tests.rs"
            | "src/native/shader/tests.rs"
            | "src/repository/rebuild_tests.rs"
            | "src/repository/cache_tests.rs"
            | "src/repository/transport/stall_tests.rs"
            | "src/repository/transport/http_tests.rs"
            | "src/extract/csharp/history/tests.rs"
            | "src/extract/csharp/history/smoke_tests.rs"
            | "src/unity/assets/native/tests.rs"
    ) {
        return [false; 5];
    }
    let facts = name.starts_with("Cargo.")
        || name.starts_with("src/binary/")
        || name.starts_with("vendor/tree-sitter-c-sharp/")
        || name.starts_with("src/extract/")
        || name.starts_with("src/native/") && !name.ends_with("jobs.rs")
        || matches!(
            name,
            "src/binary.rs" | "src/model.rs" | "src/metadata.rs"
            // Native extraction uses query::split_parameters.
            | "src/query.rs" | "src/csharp/syntax.rs" | "src/csharp/lower.rs"
            | "src/csharp/types.rs" | "src/store/data.rs" | "src/store/format.rs"
            | "src/store/payload.rs" | "src/store/segment.rs"
            | "src/cache/file_id.rs"
            | "src/metadata_archive.rs" | "src/metadata_archive/sections.rs"
            | "src/metadata_archive/bundle.rs"
            | "src/unity/references.rs"
        );
    let graph = facts
        || name.starts_with("src/discovery")
        || name.starts_with("src/unity/") && !name.starts_with("src/unity/assets/")
        || name.starts_with("src/metadata_archive")
        || matches!(
            name,
            "src/workspace.rs"
                | "src/msbuild.rs"
                | "src/selection.rs"
                | "src/documents.rs"
                | "src/cache/packages.rs"
                | "src/repository/selection.rs"
        );
    let objects = facts || name.starts_with("src/csharp/") || name.starts_with("src/unity/");
    let records = name.starts_with("Cargo.")
        || matches!(
            name,
            "src/unity/assets/parse.rs"
                | "src/unity/assets/yaml.rs"
                | "src/unity/assets/records.rs"
        );
    let layout = name.starts_with("Cargo.")
        || name.starts_with("src/binary/")
        || matches!(
            name,
            "src/binary.rs" | "src/model.rs" | "src/documents.rs" | "src/native/mod.rs"
        )
        || name.starts_with("src/cache")
        || name.starts_with("src/repository")
        || name == "src/store.rs"
        || name.starts_with("src/store/")
        || matches!(
            name,
            "src/config.rs" | "src/msbuild.rs" | "src/service/inspection.rs"
        );
    [facts, graph, objects, records, layout]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_reviewed_writers_can_reuse_the_released_layout_id() {
        for &reviewed in COMPATIBLE_WRITERS {
            assert_eq!(compatible("LAYOUT", reviewed), LAYOUT_0_1_52);
            let mut unknown = reviewed.to_owned();
            unknown.replace_range(..1, if reviewed.starts_with('0') { "1" } else { "0" });
            assert_eq!(compatible("LAYOUT", &unknown), unknown);
        }
    }

    #[test]
    fn reviewed_page_encoding_preserves_only_analysis_identity() {
        assert_eq!(
            compatible("ANALYSIS", BUFFERED_PAGE_ENCODER),
            ANALYSIS_0_1_52
        );
        assert_eq!(
            compatible("WORKSPACE", BUFFERED_PAGE_ENCODER),
            BUFFERED_PAGE_ENCODER
        );
        assert_eq!(
            compatible("ANALYSIS", COMPATIBLE_WRITERS[0]),
            COMPATIBLE_WRITERS[0]
        );
    }

    fn fingerprint(files: &[(&str, &str)]) -> [Vec<u8>; 5] {
        let mut hashes: [Vec<u8>; 5] = Default::default();
        for (name, bytes) in files {
            for (hash, include) in hashes.iter_mut().zip(domains(name)) {
                if include {
                    hash.extend(name.as_bytes());
                    hash.push(0);
                    hash.extend(bytes.as_bytes());
                }
            }
        }
        hashes
    }

    #[test]
    fn presentation_and_standalone_tests_preserve_compatibility() {
        for path in [
            "src/minify.rs",
            "src/render.rs",
            "src/store/shared_tests.rs",
            "src/metadata_archive/benchmark.rs",
            "src/native/shader/tests.rs",
        ] {
            assert_eq!(fingerprint(&[(path, "old")]), fingerprint(&[(path, "new")]));
        }
    }

    #[test]
    fn persisted_facts_and_layout_still_invalidate_their_domains() {
        for path in [
            "src/query.rs",
            "src/extract/csharp.rs",
            "src/binary/bounded.rs",
        ] {
            let before = fingerprint(&[(path, "old")]);
            let after = fingerprint(&[(path, "new")]);
            assert_ne!(before[0], after[0]);
            assert_ne!(before[1], after[1]);
        }
        let before = fingerprint(&[("src/store/shared.rs", "old")]);
        let after = fingerprint(&[("src/store/shared.rs", "new")]);
        assert_ne!(before[4], after[4]);
        let before = fingerprint(&[("src/cache/file_id.rs", "old")]);
        let after = fingerprint(&[("src/cache/file_id.rs", "new")]);
        assert_ne!(before[0], after[0]);
        assert_ne!(before[4], after[4]);
        let before = fingerprint(&[("src/cache/blobs.rs", "old")]);
        let after = fingerprint(&[("src/cache/blobs.rs", "new")]);
        assert_eq!(before[0], after[0]);
        assert_ne!(before[4], after[4]);
    }

    #[test]
    fn graph_changes_preserve_storage_but_schema_and_dependency_changes_do_not() {
        for path in ["src/workspace.rs", "src/discovery.rs", "src/unity/graph.rs"] {
            let before = fingerprint(&[(path, "old")]);
            let after = fingerprint(&[(path, "new")]);
            assert_ne!(before[1], after[1]);
            assert_eq!(before[4], after[4]);
        }
        // Repository inventories retain selected paths across compatible resets.
        // Changes to language recognition must also rebuild those selections.
        for path in [
            "Cargo.lock",
            "src/model.rs",
            "src/binary/bounded.rs",
            "src/documents.rs",
            "src/native/mod.rs",
        ] {
            assert_ne!(
                fingerprint(&[(path, "old")])[4],
                fingerprint(&[(path, "new")])[4]
            );
        }
    }
}
