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
