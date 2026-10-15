//! Offline shader corpus check. No Unity or compiler invocation.
//! cargo run --example ng_shader_probe -- /path/to/extracted/corpus > probe.json
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use sigla::{
    model::Facts,
    native::{Group, Role},
};
use std::{
    collections::BTreeMap,
    ops::Range,
    path::{Path, PathBuf},
};

fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_owned()];
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && sigla::native::language(&entry.path())
                    .is_some_and(|language| Group::of(language) == Some(Group::Shaders))
            {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}

fn check(source: &str, facts: &Facts) -> Result<()> {
    let native = facts.native.as_ref().context("missing native facts")?;
    ensure!(
        facts.declarations.len() == native.declarations.len(),
        "declaration table mismatch"
    );
    ensure!(
        facts.occurrences.len() == native.occurrences.len(),
        "occurrence table mismatch"
    );
    let span = |range: &Range<usize>| -> Result<()> {
        ensure!(
            range.start <= range.end && range.end <= source.len(),
            "invalid range {range:?}"
        );
        ensure!(
            source.is_char_boundary(range.start) && source.is_char_boundary(range.end),
            "range is not on UTF-8 boundaries: {range:?}"
        );
        Ok(())
    };
    for (declaration, info) in facts.declarations.iter().zip(&native.declarations) {
        for range in [
            &declaration.span,
            &declaration.name_span,
            &declaration.header,
            &declaration.scope,
        ] {
            span(range)?;
        }
        let region = native
            .regions
            .get(info.region as usize)
            .context("bad declaration region")?;
        ensure!(
            region.span.contains(&declaration.name_span.start),
            "declaration outside its region"
        );
        if let Some(parent) = info.parent {
            ensure!(
                (parent as usize) < facts.declarations.len(),
                "invalid parent ordinal"
            );
        }
        if info.qualifiers.starts_with("authored shader ") {
            ensure!(
                &source[declaration.name_span.clone()] == declaration.name.as_str(),
                "authored name moved"
            );
        }
    }
    let mut names = BTreeMap::<String, Vec<u32>>::new();
    let mut calls = BTreeMap::<u32, Vec<u32>>::new();
    for (index, (occurrence, info)) in facts
        .occurrences
        .iter()
        .zip(&native.occurrences)
        .enumerate()
    {
        span(&occurrence.span)?;
        if let Some(receiver) = &info.receiver {
            span(receiver)?;
        }
        if let Some(assignment) = &info.assignment {
            span(assignment)?;
        }
        let region = native
            .regions
            .get(info.region as usize)
            .context("bad occurrence region")?;
        ensure!(
            region.span.contains(&occurrence.span.start),
            "occurrence outside its region"
        );
        if let Some(local) = info.local {
            ensure!(
                facts
                    .declarations
                    .get(local as usize)
                    .is_some_and(|declaration| declaration.local()),
                "invalid local ordinal"
            );
        }
        if let Some(owner) = info.owner {
            ensure!(
                (owner as usize) < facts.declarations.len(),
                "invalid owner ordinal"
            );
            if occurrence.call {
                calls.entry(owner).or_default().push(index as u32);
            }
        }
        names
            .entry(occurrence.name.clone())
            .or_default()
            .push(index as u32);
    }
    ensure!(native.names == names, "name index mismatch");
    ensure!(native.calls == calls, "call index mismatch");
    ensure!(
        facts.occurrences.windows(2).all(|pair| {
            (pair[0].span.start, pair[0].span.end) <= (pair[1].span.start, pair[1].span.end)
        }),
        "occurrences not sorted"
    );
    for include in &native.includes {
        span(&include.span)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(
        args.next()
            .context("usage: ng_shader_probe CORPUS_DIRECTORY")?,
    );
    ensure!(
        args.next().is_none() && root.is_dir(),
        "pass one existing corpus directory"
    );
    let mut rows = Vec::new();
    let mut errors = 0;
    for path in files(&root)? {
        let language = sigla::native::language(&path).unwrap();
        let source = sigla::model::decode(&std::fs::read(&path)?, language)?;
        let facts = sigla::extract::extract(&source, language, &[], "")
            .with_context(|| path.display().to_string())?;
        check(&source, &facts).with_context(|| path.display().to_string())?;
        let native = facts.native.as_ref().unwrap();
        errors += facts.errors as usize;
        let declarations: Vec<_> = facts
            .declarations
            .iter()
            .zip(&native.declarations)
            .map(|(declaration, info)| {
                let (line, column) = sigla::model::position(&source, declaration.name_span.start);
                serde_json::json!({
                    "name": declaration.name, "kind": declaration.kind, "type": declaration.ty,
                    "start": declaration.name_span.start, "end": declaration.name_span.end,
                    "line": line, "column": column,
                    "authored": info.qualifiers.starts_with("authored shader "),
                })
            })
            .collect();
        let entry_points: Vec<_> = facts.occurrences.iter().zip(&native.occurrences)
            .filter(|(_, info)| info.role == Role::EntryPoint)
            .map(|(occurrence, _)| serde_json::json!({
                "name": occurrence.name, "start": occurrence.span.start, "end": occurrence.span.end,
                "call": occurrence.call,
            })).collect();
        let path = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        rows.push(serde_json::json!({
            "path": path, "language": language.tag(), "decoded_bytes": source.len(),
            "source_sha256": format!("{:x}", Sha256::digest(source.as_bytes())),
            "errors": facts.errors, "declarations": declarations, "entry_points": entry_points,
            "occurrences": facts.occurrences.len(), "includes": native.includes.len(),
        }));
    }
    ensure!(!rows.is_empty(), "no shader source files found");
    let report = serde_json::json!({
        "schema": 1, "files_checked": rows.len(), "files_with_parse_errors": errors,
        "span_and_index_checks": "passed", "files": rows,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
