//! Transport packs preserve the original compressed bundles and analysis identities.
use super::*;
use std::{collections::BTreeSet, io::Write};

pub(super) const MAX_PACK: u64 = 8 * 1024 * 1024;
const MAX_EXTRA: u64 = 512 * 1024;

/// Preserve complete packs; only new or changed members enter the graph again.
pub(super) fn groups(artifacts: &[Artifact], previous: &[Pack]) -> Vec<Vec<usize>> {
    let by_name: BTreeMap<_, _> = artifacts
        .iter()
        .enumerate()
        .map(|(i, a)| (a.name.as_str(), i))
        .collect();
    let mut used = BTreeSet::<usize>::new();
    let mut result = Vec::new();
    for pack in previous {
        if let Some(members) = pack
            .members
            .iter()
            .map(|n| by_name.get(n.as_str()).copied())
            .collect::<Option<Vec<_>>>()
        {
            used.extend(&members);
            result.push(members);
        }
    }
    let nodes: Vec<_> = artifacts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.manifest.origin.kind == "package")
        .collect();
    let packages: Vec<_> = nodes.iter().map(|(_, a)| &a.manifest.packages).collect();
    let keys: BTreeMap<_, _> = nodes
        .iter()
        .enumerate()
        .map(|(i, (_, a))| {
            (
                (
                    a.manifest.origin.name.as_str(),
                    a.manifest.origin.version.as_str(),
                ),
                i,
            )
        })
        .collect();
    let dependencies: Vec<BTreeSet<usize>> = packages
        .iter()
        .map(|ps| {
            ps.iter()
                .flat_map(|p| &p.dependencies)
                .filter_map(|(n, v)| keys.get(&(n.as_str(), v.as_str())).copied())
                .collect()
        })
        .collect();
    let mut closures = Vec::new();
    for root in 0..nodes.len() {
        let mut seen = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(i) = pending.pop() {
            if seen.insert(i) {
                pending.extend(&dependencies[i]);
            }
        }
        closures.push(seen);
    }
    // Shared platform modules and ubiquitous test/runtime dependencies do not
    // establish a useful download relationship between otherwise unrelated tools.
    let mut users = BTreeMap::<&str, BTreeSet<&str>>::new();
    for ps in &packages {
        for p in *ps {
            for name in p.dependencies.keys() {
                users.entry(name).or_default().insert(&p.name);
            }
        }
    }
    let useful = |name: &str| {
        !name.starts_with("com.unity.modules.") && users.get(name).is_none_or(|u| u.len() < 10)
    };
    let mut edges = Vec::new();
    for a in 0..nodes.len() {
        if used.contains(&nodes[a].0) {
            continue;
        }
        for b in a + 1..nodes.len() {
            if used.contains(&nodes[b].0) {
                continue;
            }
            let x = &nodes[a].1.manifest.origin;
            let y = &nodes[b].1.manifest.origin;
            if x.name == y.name {
                continue;
            }
            let direct = usize::from(dependencies[a].contains(&b) && useful(&y.name))
                + usize::from(dependencies[b].contains(&a) && useful(&x.name));
            let shared = if family(&x.name) == family(&y.name) {
                packages[a]
                    .iter()
                    .flat_map(|p| &p.dependencies)
                    .filter(|(n, v)| {
                        useful(n)
                            && packages[b]
                                .iter()
                                .any(|p| p.dependencies.get(*n) == Some(*v))
                    })
                    .count()
            } else {
                0
            };
            let score = direct * 4 + shared;
            if score > 0 {
                edges.push((
                    std::cmp::Reverse(score),
                    nodes[a].1.size + nodes[b].1.size,
                    a,
                    b,
                ));
            }
        }
    }
    edges.sort();
    let mut owners: Vec<_> = (0..nodes.len()).collect();
    let mut groups: Vec<_> = (0..nodes.len())
        .map(|i| {
            if used.contains(&nodes[i].0) {
                vec![]
            } else {
                vec![i]
            }
        })
        .collect();
    for (_, _, a, b) in edges {
        let (a, b) = (owners[a], owners[b]);
        if a == b {
            continue;
        }
        let merged: Vec<_> = groups[a].iter().chain(&groups[b]).copied().collect();
        let size = 1024
            + merged
                .iter()
                .map(|&i| framed_size(nodes[i].1.size))
                .sum::<u64>();
        let names: BTreeSet<_> = merged
            .iter()
            .map(|&i| &nodes[i].1.manifest.origin.name)
            .collect();
        if size > MAX_PACK
            || names.len() != merged.len()
            || merged.iter().any(|&i| {
                merged
                    .iter()
                    .filter(|j| !closures[i].contains(j))
                    .map(|&j| nodes[j].1.size)
                    .sum::<u64>()
                    > MAX_EXTRA
            })
        {
            continue;
        }
        for &i in &merged {
            owners[i] = a;
        }
        groups[a] = merged;
        groups[b].clear();
    }
    result.extend(
        groups
            .into_iter()
            .filter(|g| !g.is_empty())
            .map(|g| g.into_iter().map(|i| nodes[i].0).collect::<Vec<_>>()),
    );
    result.extend(
        artifacts
            .iter()
            .enumerate()
            .filter(|(i, a)| !used.contains(i) && a.manifest.origin.kind != "package")
            .map(|(i, _)| vec![i]),
    );
    for group in &mut result {
        group.sort_by_key(|&i| &artifacts[i].name);
    }
    result.sort_by_key(|g| &artifacts[g[0]].name);
    result
}

fn family(name: &str) -> &str {
    let name = name.strip_prefix("com.unity.").unwrap_or(name);
    match name.split('.').next().unwrap_or(name) {
        "sdk" | "sysroot" | "toolchain" => "toolchain",
        family => family,
    }
}

fn framed_size(size: u64) -> u64 {
    512 + size.div_ceil(512) * 512
}

pub(super) fn build(
    members: &[&Artifact],
    output: &Path,
    mut source: impl FnMut(&Artifact) -> Result<PathBuf>,
) -> Result<Pack> {
    let mut file = tempfile::NamedTempFile::new_in(output)?;
    let mut tar = tar::Builder::new(file.as_file_mut());
    for member in members {
        let path = source(member)?;
        bundle::verify_digest(&path, member)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(member.size);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, &member.name, fs::File::open(path)?)?;
    }
    tar.into_inner()?.flush()?;
    file.as_file().sync_all()?;
    let hash = digest(file.path())?;
    let name = format!("pack-{}.tar", hex(&hash));
    let size = file.as_file().metadata()?.len();
    ensure!(size <= MAX_PACK, "Package pack exceeds size bound");
    file.persist(output.join(&name))?;
    Ok(Pack {
        name,
        hash,
        size,
        release: String::new(),
        members: members.iter().map(|a| a.name.clone()).collect(),
    })
}

pub(super) fn unpack(path: &Path, output: &Path, pack: &Pack, catalog: &Catalog) -> Result<()> {
    ensure!(
        fs::metadata(path)?.len() == pack.size && digest(path)? == pack.hash,
        "Pack digest or size mismatch"
    );
    let mut pending: BTreeMap<_, _> = pack
        .members
        .iter()
        .map(|name| {
            let artifact = catalog
                .artifacts
                .iter()
                .find(|a| &a.name == name)
                .context("Unknown pack member")?;
            Ok((name.as_str(), artifact))
        })
        .collect::<Result<_>>()?;
    fs::create_dir_all(output)?;
    for entry in tar::Archive::new(fs::File::open(path)?).entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        let artifact = pending
            .remove(name.as_str())
            .context("Unexpected or duplicate pack member")?;
        ensure!(
            entry.header().entry_type().is_file() && entry.size() == artifact.size,
            "Invalid pack member"
        );
        safe(&name)?;
        ensure!(!name.contains('/'), "Invalid pack member name");
        let destination = output.join(name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        std::io::copy(&mut entry, &mut file)?;
        bundle::verify_digest(&destination, artifact)?;
    }
    ensure!(pending.is_empty(), "Pack is missing members");
    Ok(())
}
