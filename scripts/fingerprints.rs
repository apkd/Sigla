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
    let layout = name.starts_with("src/cache")
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
}
