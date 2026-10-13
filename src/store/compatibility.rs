//! Audited transition from the whole-program fingerprint to scoped versions.
//! These releases use identical extraction and record formats. Future changes
//! to those inputs automatically disable this compatibility window.
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
    version == super::ANALYSIS_VERSION.as_bytes()
        || super::ANALYSIS_VERSION == BASELINE
            && LEGACY.iter().any(|known| version == known.as_bytes())
}

pub(crate) fn legacy_workspace() -> bool {
    super::ANALYSIS_VERSION == BASELINE && env!("SIGLA_WORKSPACE_FINGERPRINT") == WORKSPACE
}
