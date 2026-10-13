//! CPU-only extraction scaling; no binding, services, file IO, or caches.
//! Run the same example on the base and patched revisions:
//! cargo run --release --example csharp_extraction_probe -- 1000 2000 4000 8000
//! Each workload gets one warmup and five measured extractions. Generation,
//! validation, serialization, hashing, and destruction are outside the timer.
//! The digest includes Facts AND FileSyntax (Facts alone skips C# syntax).
use anyhow::{Context, Result, ensure};
use sigla::{
    extract,
    model::{Facts, Language},
};
use std::{fmt::Write as _, hint::black_box, time::Instant};

fn source(shape: &str, siblings: usize) -> String {
    let mut source = if shape == "top-level" {
        String::new()
    } else {
        String::from("namespace Probe; class Wide {\n")
    };
    for i in 0..siblings {
        match shape {
            "fields" => writeln!(source, "int F{i:08}, G{i:08};").unwrap(),
            "methods" => writeln!(source, "void M{i:08}(int p) {{ int x = p; }}").unwrap(),
            "top-level" => writeln!(source, "int v{i:08} = 0;").unwrap(),
            _ => unreachable!(),
        }
    }
    source.push_str(if shape == "top-level" {
        "class Tail { int field; }\n"
    } else {
        "}\n"
    });
    source
}

fn digest(facts: &Facts) -> Result<blake3::Hash> {
    ensure!(!facts.errors, "generated C# has parse errors");
    let syntax = facts.csharp.as_ref().context("missing C# syntax")?;
    ensure!(
        syntax.headers.len() == facts.declarations.len(),
        "declaration/header counts differ"
    );
    Ok(blake3::hash(&postcard::to_allocvec(&(facts, syntax))?))
}

fn main() -> Result<()> {
    let mut sizes = std::env::args()
        .skip(1)
        .map(|arg| {
            arg.parse::<usize>()
                .with_context(|| format!("invalid sibling count: {arg}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if sizes.is_empty() {
        sizes = vec![1000, 2000, 4000, 8000];
    }
    ensure!(
        sizes.iter().all(|&size| size > 0),
        "counts must be positive"
    );
    sizes.sort_unstable();
    sizes.dedup();
    eprintln!(
        "sigla {}; one warmup, five samples; median; drop excluded",
        env!("CARGO_PKG_VERSION")
    );
    println!(
        "shape,siblings,bytes,declarations,headers,expressions,median_ms,ns_per_declaration,digest"
    );
    for shape in ["fields", "methods", "top-level"] {
        for &siblings in &sizes {
            let source = source(shape, siblings);
            let warmup = extract::extract(&source, Language::CSharp, &[], "")?;
            let expected = digest(&warmup)?;
            let syntax = warmup.csharp.as_ref().unwrap();
            let declarations = warmup.declarations.len();
            let headers = syntax.headers.len();
            let expressions = syntax.expressions.len();
            drop(warmup);
            let mut samples = Vec::with_capacity(5);
            for _ in 0..5 {
                let start = Instant::now();
                let facts = black_box(extract::extract(
                    black_box(source.as_str()),
                    Language::CSharp,
                    &[],
                    "",
                )?);
                samples.push(start.elapsed());
                ensure!(
                    digest(&facts)? == expected,
                    "extraction output changed between runs"
                );
                drop(facts);
            }
            samples.sort_unstable();
            let median = samples[samples.len() / 2].as_secs_f64();
            println!(
                "{shape},{siblings},{},{declarations},{headers},{expressions},{:.3},{:.1},{expected}",
                source.len(),
                median * 1000.0,
                median * 1e9 / declarations as f64,
            );
        }
    }
    Ok(())
}
