//! compare normalized member signatures with the development SRM oracle.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
#[derive(Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct Signature {
    qualified: String,
    kind: String,
    ty: String,
    parameters: Vec<String>,
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let assembly = args.next().context("Expected assembly path")?;
    let oracle = args.next().context("Expected oracle JSON path")?;
    let mut expected: Vec<Signature> = serde_json::from_slice(&std::fs::read(oracle)?)?;
    let facts = sigla::metadata::extract(std::path::Path::new(&assembly))?;
    let mut actual: Vec<_> = facts
        .members
        .into_iter()
        .filter(|m| {
            matches!(
                m.kind.as_str(),
                "method" | "constructor" | "field" | "property"
            )
        })
        .map(|m| Signature {
            qualified: m.qualified,
            kind: m.kind,
            ty: m.ty,
            parameters: m.parameters,
        })
        .collect();
    expected.sort();
    actual.sort();
    ensure!(
        actual.len() == expected.len(),
        "Member counts differ: Rust {}, oracle {}",
        actual.len(),
        expected.len()
    );
    for (actual, expected) in actual.iter().zip(&expected) {
        ensure!(
            actual == expected,
            "Signature mismatch:\nRust: {actual:?}\nOracle: {expected:?}"
        );
    }
    println!("{}: {} member signatures match SRM", assembly, actual.len());
    Ok(())
}
