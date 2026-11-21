use super::*;
use crate::cache::blobs::{OwnedTree, Store};
use std::io::{Seek, SeekFrom};

const MAX_SECTIONS: usize = 4;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PATHS: usize = 32 * 1024;
const RANGE_BYTES: u64 = 8 * 1024 * 1024;
const GAP_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy)]
enum Source<'a> {
    Archive(&'a Path),
    Sections(&'a Path),
    Remote,
}

struct Stage<'section, 'store> {
    index: usize,
    section: &'section Section,
    directory: tempfile::TempDir,
    output: PathBuf,
    tree: OwnedTree<'store>,
    bytes: u64,
    paths: usize,
}

struct Pending<'section, 'store> {
    stages: Vec<Stage<'section, 'store>>,
    results: BTreeMap<usize, Result<PathBuf>>,
    bytes: u64,
    paths: usize,
    max_bytes: u64,
    max_paths: usize,
}

impl<'section, 'store> Pending<'section, 'store> {
    fn new(max_bytes: u64, max_paths: usize) -> Self {
        Self {
            stages: Vec::new(),
            results: BTreeMap::new(),
            bytes: 0,
            paths: 0,
            max_bytes,
            max_paths,
        }
    }

    fn prepare_file(
        &mut self,
        parent: &Path,
        store: &'store Store,
        index: usize,
        section: &'section Section,
        source: (&Path, u64),
    ) -> Result<Stage<'section, 'store>> {
        let mut file = fs::File::open(source.0)?;
        file.seek(SeekFrom::Start(source.1))?;
        self.prepare(parent, store, index, section, &mut file.take(section.size))
    }

    fn prepare(
        &mut self,
        parent: &Path,
        store: &'store Store,
        index: usize,
        section: &'section Section,
        reader: &mut dyn Read,
    ) -> Result<Stage<'section, 'store>> {
        let directory = tempfile::tempdir_in(parent)?;
        let output = directory.path().join("contents");
        let mut tree = store.owned_tree(&output)?;
        let mut budget = (0, 0);
        let operation = crate::telemetry::operation("metadata_unpack", None, Some(&section.role));
        let frame = sections::authenticated(reader, section.size, &section.hash, |reader| {
            bundle::inspect_section_with(
                reader,
                Some(&output),
                section,
                Some(&mut tree),
                |frame, marker_size| {
                    budget = (
                        frame
                            .entries
                            .iter()
                            .fold(marker_size, |bytes, entry| bytes.saturating_add(entry.size)),
                        frame.entries.iter().fold(1usize, |paths, entry| {
                            paths.saturating_add(Path::new(&entry.path).components().count())
                        }),
                    );
                    if self.bytes.saturating_add(budget.0) > self.max_bytes
                        || self.paths.saturating_add(budget.1) > self.max_paths
                        || self.stages.len() == MAX_SECTIONS
                    {
                        let _wait = operation.wait("publish_previous_sections");
                        self.flush(parent);
                    }
                    Ok(())
                },
            )
        });
        operation.finish(if frame.is_ok() { "success" } else { "failure" });
        let frame = frame?;
        tree.write(
            &output.join("section.bin"),
            0o600,
            envelope(&binary::encode(&frame)?).as_slice(),
        )?;
        Ok(Stage {
            index,
            section,
            directory,
            output,
            tree,
            bytes: budget.0,
            paths: budget.1,
        })
    }

    fn push(&mut self, parent: &Path, stage: Stage<'section, 'store>) {
        self.bytes = self.bytes.saturating_add(stage.bytes);
        self.paths = self.paths.saturating_add(stage.paths);
        self.stages.push(stage);
        // One large section is supported without retaining another beside it.
        if self.bytes >= self.max_bytes || self.paths >= self.max_paths {
            self.flush(parent);
        }
    }

    fn flush(&mut self, parent: &Path) {
        if self.stages.is_empty() {
            return;
        }
        let (trees, stages): (Vec<_>, Vec<_>) = self
            .stages
            .drain(..)
            .map(|stage| {
                (
                    stage.tree,
                    (stage.index, stage.section, stage.directory, stage.output),
                )
            })
            .unzip();
        let operation = crate::telemetry::operation("metadata_import", None, None);
        let imported = OwnedTree::publish_together(trees);
        operation.finish(
            if imported
                .as_ref()
                .is_ok_and(|results| results.iter().all(Result::is_ok))
            {
                "success"
            } else {
                "failure"
            },
        );
        let imported = match imported {
            Ok(results) => results,
            Err(error) => {
                let error = Arc::new(error);
                (0..stages.len())
                    .map(|_| Err(crate::diagnostics::shared_error(error.clone())))
                    .collect()
            }
        };
        for ((index, section, _directory, output), result) in stages.into_iter().zip(imported) {
            let result = result.and_then(|stats| {
                tracing::debug!(section = %section.role, compressed_bytes = section.size, files = stats.files, hashed_bytes = stats.hashed_bytes, "Metadata section imported");
                let destination = parent.join(hex(&section.hash));
                if destination.exists() {
                    crate::cache::exchange(&output, &destination)?;
                } else {
                    fs::rename(&output, &destination)?;
                }
                fs::File::open(parent)?.sync_all()?;
                Ok(destination)
            });
            self.results.insert(index, result);
        }
        self.bytes = 0;
        self.paths = 0;
    }
}

fn bounds(artifact: &Artifact, section: &Section) -> Result<(u64, u64)> {
    let start = artifact
        .header_size
        .checked_add(section.offset)
        .context("Section offset overflow")?;
    let end = start
        .checked_add(section.size)
        .and_then(|end| end.checked_sub(1))
        .context("Section end overflow")?;
    Ok((start, end))
}

/// A partial response can stream after releasing the source gate. A full
/// fallback stays under that gate until its verified durable copy is available.
fn download(
    root: &Path,
    artifact: &Artifact,
    start: u64,
    end: u64,
    fetch: &impl Fn(u64, u64) -> Result<reqwest::blocking::Response>,
) -> Result<Option<reqwest::blocking::Response>> {
    acquisition::serialized(&format!("section-source:{}", root.display()), || {
        Ok((|| {
            let complete = root.join("source.sigla");
            if complete.is_file() {
                return Ok(None);
            }
            fs::create_dir_all(root)?;
            let response = fetch(start, end)?;
            if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                ensure!(
                    response
                        .headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|value| value.to_str().ok())
                        == Some(format!("bytes {start}-{end}/{}", artifact.size).as_str()),
                    "Invalid metadata range response"
                );
                return Ok(Some(response));
            }
            ensure!(
                response.status() == reqwest::StatusCode::OK,
                "Unsupported metadata range response"
            );
            let mut file = tempfile::NamedTempFile::new_in(root)?;
            acquisition::stream(response, |reader| {
                sections::authenticated(reader, artifact.size, &artifact.hash, |reader| {
                    std::io::copy(reader, &mut file)?;
                    Ok(())
                })
            })?;
            file.as_file().sync_all()?;
            file.persist(&complete)?;
            fs::File::open(root)?.sync_all()?;
            Ok(None)
        })())
    })?
}

fn ranges(
    root: &Path,
    artifact: &Artifact,
    selected: &[&Section],
    fetch: &impl Fn(u64, u64) -> Result<reqwest::blocking::Response>,
) -> Result<Option<tempfile::TempDir>> {
    let start = bounds(artifact, selected[0])?.0;
    let end = bounds(artifact, selected.last().unwrap())?.1;
    acquisition::shared_versioned(
        &format!("section-ranges:{}:{start}-{end}", root.display()),
        &artifact.release,
        || {
            crate::telemetry::record("metadata_download", None, Some(&artifact.name), || {
                let Some(response) = download(root, artifact, start, end, fetch)? else {
                    return Ok(None);
                };
                let stage = tempfile::tempdir_in(root)?;
                acquisition::stream(response, |reader| {
                    let mut position = selected[0].offset;
                    for &section in selected {
                        let gap = section
                            .offset
                            .checked_sub(position)
                            .context("Overlapping metadata ranges")?;
                        ensure!(
                            std::io::copy(&mut reader.take(gap), &mut std::io::sink())? == gap,
                            "Truncated metadata range gap"
                        );
                        let mut file = fs::File::create(stage.path().join(hex(&section.hash)))?;
                        // Each member authenticates independently during preparation.
                        ensure!(
                            std::io::copy(&mut reader.take(section.size), &mut file)?
                                == section.size,
                            "Truncated metadata range"
                        );
                        position = section
                            .offset
                            .checked_add(section.size)
                            .context("Section end overflow")?;
                    }
                    ensure!(
                        reader.read(&mut [0])? == 0,
                        "Unexpected metadata range tail"
                    );
                    Ok(())
                })?;
                Ok(Some(stage))
            })
        },
    )
}

fn neighboring(first: &Section, previous: &Section, next: &Section) -> bool {
    next.offset
        .saturating_sub(previous.offset.saturating_add(previous.size))
        <= GAP_BYTES
        && next
            .offset
            .saturating_add(next.size)
            .saturating_sub(first.offset)
            <= RANGE_BYTES
}

fn prepare_group(
    cache: &Path,
    root: &Path,
    artifact: &Artifact,
    selected: &[(usize, &Section)],
    source: Source<'_>,
    fetch: &impl Fn(u64, u64) -> Result<reqwest::blocking::Response>,
) -> Result<BTreeMap<usize, Result<PathBuf>>> {
    let operation = crate::telemetry::operation("metadata_materialize", None, Some(&artifact.name));
    let result = (|| {
        let parent = root.join("sections");
        fs::create_dir_all(&parent)?;
        let store = Store::open(cache)?;
        let mut pending = Pending::new(MAX_BYTES, MAX_PATHS);
        for &(index, section) in selected {
            let result = (|| {
                acquisition::check_running()?;
                let (start, end) = bounds(artifact, section)?;
                match source {
                    Source::Sections(directory) => pending.prepare_file(
                        &parent,
                        &store,
                        index,
                        section,
                        (&directory.join(hex(&section.hash)), 0),
                    ),
                    Source::Archive(path) => {
                        pending.prepare_file(&parent, &store, index, section, (path, start))
                    }
                    Source::Remote => {
                        let complete = root.join("source.sigla");
                        if complete.is_file() {
                            return pending.prepare_file(
                                &parent,
                                &store,
                                index,
                                section,
                                (&complete, start),
                            );
                        }
                        crate::telemetry::record(
                            "metadata_download",
                            None,
                            Some(&section.role),
                            || {
                                if let Some(response) = download(root, artifact, start, end, fetch)?
                                {
                                    acquisition::stream(response, |reader| {
                                        pending.prepare(&parent, &store, index, section, reader)
                                    })
                                } else {
                                    pending.prepare_file(
                                        &parent,
                                        &store,
                                        index,
                                        section,
                                        (&complete, start),
                                    )
                                }
                            },
                        )
                    }
                }
            })();
            match result {
                // Transfer verification finishes before the tree joins publication.
                Ok(stage) => pending.push(&parent, stage),
                Err(error) => {
                    pending.results.insert(index, Err(error));
                }
            }
        }
        pending.flush(&parent);
        Ok(pending.results)
    })();
    operation.finish(
        if result
            .as_ref()
            .is_ok_and(|results: &BTreeMap<usize, Result<PathBuf>>| {
                results.values().all(Result::is_ok)
            })
        {
            "success"
        } else {
            "failure"
        },
    );
    result
}

fn publish_group(
    cache: &Path,
    root: &Path,
    artifact: &Artifact,
    group: &[&Section],
    version: &str,
    source: Source<'_>,
    fetch: &impl Fn(u64, u64) -> Result<reqwest::blocking::Response>,
) -> BTreeMap<String, Result<PathBuf>> {
    let identities: Vec<_> = group
        .iter()
        .map(|section| {
            root.join("sections")
                .join(hex(&section.hash))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let acquired = acquisition::shared_many_versioned(&identities, version, |indices| {
        let mut completed = BTreeMap::new();
        let mut pending = Vec::new();
        for &index in indices {
            let section = group[index];
            let destination = root.join("sections").join(hex(&section.hash));
            if section_complete(&destination, section) {
                completed.insert(index, Ok(destination));
            } else {
                pending.push((index, section));
            }
        }
        if !pending.is_empty() {
            match prepare_group(cache, root, artifact, &pending, source, fetch) {
                Ok(prepared) => completed.extend(prepared),
                Err(error) => {
                    let error = Arc::new(error);
                    for &(index, _) in &pending {
                        completed
                            .insert(index, Err(crate::diagnostics::shared_error(error.clone())));
                    }
                }
            }
        }
        indices
            .iter()
            .map(|index| completed.remove(index).unwrap())
            .collect()
    });
    match acquired {
        Ok(acquired) => group
            .iter()
            .zip(acquired)
            .map(|(section, result)| (section.role.clone(), result))
            .collect(),
        Err(error) => {
            let error = Arc::new(error);
            group
                .iter()
                .map(|section| {
                    (
                        section.role.clone(),
                        Err(crate::diagnostics::shared_error(error.clone())),
                    )
                })
                .collect()
        }
    }
}

fn pending_sections<'a>(
    root: &Path,
    selected: &[&'a Section],
    version: &str,
    results: &mut BTreeMap<String, Result<PathBuf>>,
) -> Vec<&'a Section> {
    selected
        .iter()
        .filter_map(|&section| {
            let destination = root.join("sections").join(hex(&section.hash));
            if section_complete(&destination, section) {
                results.insert(section.role.clone(), Ok(destination));
            } else if let Err(error) =
                acquisition::check_cached_failure(&destination.to_string_lossy(), version)
            {
                results.insert(section.role.clone(), Err(error));
            } else {
                return Some(section);
            }
            None
        })
        .collect()
}

fn group_end(selected: &[&Section], at: usize, local: bool) -> usize {
    let mut end = at + 1;
    while end < selected.len() {
        if local {
            if end - at == MAX_SECTIONS {
                break;
            }
        } else if !neighboring(selected[at], selected[end - 1], selected[end]) {
            break;
        }
        end += 1;
    }
    end
}

/// Transport spans obey byte and gap bounds. Their private compressed files
/// outlive transport admission; publication then admits at most four trees.
pub(in crate::metadata_archive) fn many(
    cache: &Path,
    artifact: &Artifact,
    catalog: &Catalog,
    selected: &[&Section],
    leases: &crate::cache::views::LeaseSet,
    fetch: impl Fn(u64, u64) -> Result<reqwest::blocking::Response>,
) -> BTreeMap<String, Result<PathBuf>> {
    let root = cache.join("unity-metadata").join(hex(&artifact.hash));
    if let Err(error) = leases.retain(&root) {
        let error = Arc::new(error);
        return selected
            .iter()
            .map(|section| {
                (
                    section.role.clone(),
                    Err(crate::diagnostics::shared_error(error.clone())),
                )
            })
            .collect();
    }
    let version = catalog
        .packs
        .iter()
        .find(|pack| pack.members.contains(&artifact.name))
        .map_or(artifact.release.as_str(), |pack| pack.name.as_str());
    let mut result = BTreeMap::new();
    let mut missing = pending_sections(&root, selected, version, &mut result);
    if result.values().any(Result::is_ok) {
        leases.record_metadata_use(&root);
    }
    if missing.is_empty() {
        return result;
    }
    missing.sort_by_key(|section| section.offset);
    missing.dedup_by(|a, b| a.role == b.role);
    let complete = root.join("source.sigla");
    // Pack acquisition finishes before destination locks and publication admission.
    let packed = if complete.is_file() {
        Ok(None)
    } else {
        packed_source(cache, REPOSITORY, catalog, artifact, leases)
    };
    let packed = match packed {
        Ok(source) => source,
        Err(error) => {
            let error = Arc::new(error);
            for section in missing {
                result.insert(
                    section.role.clone(),
                    Err(crate::diagnostics::shared_error(error.clone())),
                );
            }
            return result;
        }
    };
    let mut at = 0;
    while at < missing.len() {
        let end = group_end(&missing, at, packed.is_some() || complete.is_file());
        let group = &missing[at..end];
        let identity = format!(
            "section-batch:{}:{}-{}",
            root.display(),
            group[0].offset,
            group.last().unwrap().offset
        );
        let published = acquisition::serialized_versioned(&identity, version, || {
            // Recheck races and cooldowns before deciding which bytes to acquire.
            let pending = pending_sections(&root, group, version, &mut result);
            let mut at = 0;
            while at < pending.len() {
                let local = packed.is_some() || complete.is_file();
                let end = group_end(&pending, at, local);
                let span = &pending[at..end];
                let compressed = if span.len() > 1 && !local {
                    ranges(&root, artifact, span, &fetch)
                } else {
                    Ok(None)
                };
                match compressed {
                    Ok(compressed) => {
                        let source = if let Some(compressed) = &compressed {
                            Source::Sections(compressed.path())
                        } else if let Some(packed) = &packed {
                            Source::Archive(packed)
                        } else if complete.is_file() {
                            Source::Archive(&complete)
                        } else {
                            Source::Remote
                        };
                        for group in span.chunks(MAX_SECTIONS) {
                            result.extend(publish_group(
                                cache, &root, artifact, group, version, source, &fetch,
                            ));
                        }
                    }
                    Err(error) => {
                        // The range owns this cooldown. Different selections may retry.
                        let error = Arc::new(error);
                        for section in span {
                            result.insert(
                                section.role.clone(),
                                Err(crate::diagnostics::shared_error(error.clone())),
                            );
                        }
                    }
                }
                at = end;
            }
            Ok(())
        });
        if let Err(error) = published {
            let error = Arc::new(error);
            for section in group {
                result.insert(
                    section.role.clone(),
                    Err(crate::diagnostics::shared_error(error.clone())),
                );
            }
        }
        at = end;
    }
    if result.values().any(Result::is_ok) {
        leases.record_metadata_use(&root);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_limits_publish_earlier_trees_and_isolate_large_sections() {
        let output = tempfile::tempdir().unwrap();
        let mut builder = bundle::Builder::new(
            Origin {
                kind: "package".into(),
                name: "fixture".into(),
                version: "1.0.0".into(),
                revision: String::new(),
                url: String::new(),
                integrity: None,
            },
            vec![vec![]],
        )
        .unwrap();
        for (name, size) in [("a", 64), ("b", 4096), ("c", 64)] {
            builder
                .add(
                    format!("packages/{name}/file.txt"),
                    vec![b'x'; size],
                    "package",
                )
                .unwrap();
        }
        let artifact = builder.finish(output.path()).unwrap();
        let source = output.path().join(&artifact.name);
        let first = &artifact.sections[0];
        let mut input = fs::File::open(&source).unwrap();
        input
            .seek(SeekFrom::Start(bounds(&artifact, first).unwrap().0))
            .unwrap();
        let frame = bundle::inspect_section(input.take(first.size), None, first, None).unwrap();
        let small_bytes = envelope(&binary::encode(&frame).unwrap()).len() as u64
            + frame.entries.iter().map(|entry| entry.size).sum::<u64>();
        let small_paths = 1 + frame
            .entries
            .iter()
            .map(|entry| Path::new(&entry.path).components().count())
            .sum::<usize>();
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        for (name, bytes, paths) in [
            ("bytes", small_bytes * 2, usize::MAX),
            ("paths", u64::MAX, small_paths * 2),
        ] {
            let parent = cache.path().join(name);
            fs::create_dir(&parent).unwrap();
            let mut pending = Pending::new(bytes, paths);
            for (index, section) in artifact.sections.iter().enumerate() {
                let stage = pending
                    .prepare_file(
                        &parent,
                        &store,
                        index,
                        section,
                        (&source, bounds(&artifact, section).unwrap().0),
                    )
                    .unwrap();
                pending.push(&parent, stage);
                if index == 0 {
                    assert!(pending.results.is_empty());
                } else {
                    assert!(
                        parent
                            .join(hex(&artifact.sections[0].hash))
                            .join("section.bin")
                            .is_file()
                    );
                    assert!(
                        parent
                            .join(hex(&artifact.sections[1].hash))
                            .join("section.bin")
                            .is_file()
                    );
                }
            }
            assert!(!parent.join(hex(&artifact.sections[2].hash)).exists());
            pending.flush(&parent);
            assert_eq!(pending.results.len(), artifact.sections.len());
            assert!(
                pending
                    .results
                    .into_values()
                    .all(|result| result.unwrap().join("section.bin").is_file())
            );
        }
    }
}
