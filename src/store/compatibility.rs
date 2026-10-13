//! Audited transition from the whole-program fingerprint to scoped versions.
//! BASELINE and WORKSPACE are canonical identities, not latest source hashes.
//! Source changes keep these gates open only via exact audited aliases in build.rs.
const BASELINE: &str = "629b05383367eea33b4dc3d889476d1322538c00db095b6028d7581ec525b2e0";
const WORKSPACE: &str = "441f5755af01b491e09beba20b3271ee63d640c3360af7722fc0110b4504e83a";
const LEGACY: &[&str] = &[
    "d70ac251b161d05505fdc5790766a45bdd917c9e97093b7df3751e7479131b09", // 0.1.14
    "06b5ba49a564c7fe0766d37fcaa9168238c7e8ad4be30a5665c05ca388108116", // 0.1.15
    "11f23979770f9810c70435770598b3fabf7c6b90ebdcd0d2442305d51e417a83", // cleanup fork
    "cf33cae1ec0afc0d405dfa1801a9253819ba77afde6cf13f743c1fdcd0f6f006", // 0.1.16
    "9684d497725f24e3b9c29fa6b6a5c616b195960ca69966a5a60cbfcc75f5bca2", // 0.1.17
];

pub(crate) fn accepts(version: &[u8]) -> bool {
    accepts_at(super::ANALYSIS_VERSION, version)
}

fn accepts_at(current: &str, version: &[u8]) -> bool {
    version == current.as_bytes()
        || current == BASELINE && LEGACY.iter().any(|known| version == known.as_bytes())
}

pub(crate) fn legacy_workspace() -> bool {
    legacy_workspace_at(super::ANALYSIS_VERSION, env!("SIGLA_WORKSPACE_FINGERPRINT"))
}

fn legacy_workspace_at(analysis: &str, workspace: &str) -> bool {
    analysis == BASELINE && workspace == WORKSPACE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analysis_gate_is_exact_and_baseline_scoped() {
        let prefixed = format!("sigla-shared-3:{BASELINE}");
        for current in [BASELINE, "unreviewed-analysis"] {
            assert!(accepts_at(current, current.as_bytes()));
            assert_eq!(
                accepts_at(current, BASELINE.as_bytes()),
                current == BASELINE
            );
            for version in LEGACY {
                assert_eq!(accepts_at(current, version.as_bytes()), current == BASELINE);
            }
            for version in ["", WORKSPACE, &BASELINE[..63], prefixed.as_str()] {
                assert!(!accepts_at(current, version.as_bytes()));
            }
        }
    }

    #[test]
    fn legacy_workspace_requires_both_canonical_identities() {
        assert!(legacy_workspace_at(BASELINE, WORKSPACE));
        assert!(!legacy_workspace_at(BASELINE, "unreviewed-workspace"));
        assert!(!legacy_workspace_at("unreviewed-analysis", WORKSPACE));
        assert!(!legacy_workspace_at(
            "unreviewed-analysis",
            "unreviewed-workspace"
        ));
    }
}
