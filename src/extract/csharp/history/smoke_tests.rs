//! Diagnostic and acceptance probes for the smoke-test C# failures.
use super::*;

#[test]
fn smoke_parse_recovery_reports_sites_without_weakening_safety() {
    let valid = "class Worker { int Existing() { return Known(); } int Known() { return 1; } }";
    let complete = extract(valid).unwrap();
    assert!(!complete.occurrences.is_empty());
    let source = "class Worker { int Existing() { return Known(); } int Known() { return 1; } void Broken( }";
    let facts = extract(source).unwrap();
    let reason = facts
        .incomplete
        .as_deref()
        .expect("damaged source must remain incomplete");
    assert!(reason.contains("historical parse recovery"), "{reason}");
    assert!(
        reason.contains(" at ") && reason.contains("(bytes "),
        "{reason}"
    );
    assert!(!facts.declarations.is_empty());
    assert!(facts.declarations.iter().all(|d| d.uncertain));
    assert!(facts.occurrences.is_empty());
}

#[test]
fn smoke_valid_csharp_does_not_gain_recovery_diagnostics() {
    let facts = extract("class Worker { int Run() { return 1; } }").unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert!(facts.declarations.iter().all(|d| !d.uncertain));
}

#[test]
fn required_identifiers_and_modifiers_parse_in_history() {
    for source in [
        "class W { readonly HashSet<string> required = new(); void Target(string path) { required.Add(path); } }",
        "class W { void Target(HashSet<string> required, string path) { required.Add(path); } }",
        "class W { void Target(string path) { var required = new HashSet<string>(); required.Add(path); } }",
        "class W { void Target(HashSet<string> required, string path) { if (path != null) required.Add(path); } }",
        "class W { void Target(HashSet<string> required, string path) { void Require() { required.Add(path); } Require(); } }",
        "class W { public required string Name { get; init; } void Target() {} }",
        "class W { void Target() { @required.Add(path); } }",
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/managed/Program.cs")),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/managed/TrackedFileSystem.cs"
        )),
    ] {
        let facts = extract(source).unwrap();
        assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
        assert!(!facts.declarations.is_empty());
        assert!(facts.declarations.iter().all(|d| !d.uncertain));
    }

    let facts = extract(
        "class W { public required string Name { get; init; } void Target() { required.Add(Name); } }",
    )
    .unwrap();
    assert!(facts.declarations.iter().any(|d| {
        d.name == "Name" && d.kind == "property" && d.modifiers.iter().any(|m| m == "required")
    }));
    assert!(facts.occurrences.iter().any(|o| o.name == "Add" && o.call));
}
