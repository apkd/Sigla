//! Cold migration of positively identified derived caches, under root ownership.
use anyhow::{Result, ensure};
use std::{fs, path::Path};

fn directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "Invalid cache directory: {}",
                path.display()
            );
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn legacy_analysis(path: &Path) -> Result<()> {
    if !directory(path)? {
        return Ok(());
    }
    let marker = path.join("sigla-legacy-analysis");
    let interrupted = match fs::symlink_metadata(&marker) {
        Ok(m) => {
            ensure!(
                m.is_file() && !m.file_type().is_symlink() && m.len() < 64,
                "Invalid migration marker"
            );
            ensure!(
                fs::read(&marker)? == b"sigla-facts-15",
                "Unknown migration marker"
            );
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if interrupted {
        return remove_analysis(path);
    }
    for name in ["data.mdb", "lock.mdb"] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(m) => ensure!(
                m.is_file() && !m.file_type().is_symlink(),
                "Invalid legacy analysis file"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    // SAFETY: caller owns the service-cache lock and has not opened workspaces.
    let env = unsafe {
        heed::EnvOpenOptions::new()
            .max_dbs(7)
            .flags(heed::EnvFlags::READ_ONLY)
            .open(path)
    }?;
    let recognized = {
        let tx = env.read_txn()?;
        let table =
            env.open_database::<heed::types::Str, heed::types::Bytes>(&tx, Some("metadata"))?;
        table
            .map(|table| {
                table
                    .get(&tx, "format")
                    .map(|v| v == Some(b"sigla-facts-15".as_slice()))
            })
            .transpose()?
            .unwrap_or(false)
    };
    env.prepare_for_closing().wait();
    if recognized {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)?;
        file.write_all(b"sigla-facts-15")?;
        file.sync_all()?;
        fs::File::open(path)?.sync_all()?;
        remove_analysis(path)?;
    }
    Ok(())
}

fn remove_analysis(path: &Path) -> Result<()> {
    // The durable marker makes a partially removed cache recognizable on restart.
    for name in ["data.mdb", "lock.mdb"] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(m) => {
                ensure!(
                    m.is_file() && !m.file_type().is_symlink(),
                    "Invalid legacy analysis file"
                );
                fs::remove_file(path.join(name))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    fs::File::open(path)?.sync_all()?;
    fs::remove_file(path.join("sigla-legacy-analysis"))?;
    if fs::read_dir(path)?.next().is_none() {
        fs::remove_dir(path)?;
    }
    Ok(())
}

fn scoped_analysis(root: &Path) -> Result<()> {
    if !directory(root)? {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            legacy_analysis(&entry.path())?;
        }
    }
    Ok(())
}

pub fn migrate(cache: &Path) -> Result<()> {
    legacy_analysis(&cache.join("assemblies"))?;
    scoped_analysis(cache)?;
    let repositories = cache.join("repositories");
    if !directory(&repositories)? {
        return Ok(());
    }
    for entry in fs::read_dir(&repositories)? {
        let entry = entry?;
        ensure!(directory(&entry.path())?, "Missing repository cache");
        let state_path = entry.path().join("state.json");
        let metadata = match fs::symlink_metadata(&state_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Invalid selector metadata"
        );
        let state: crate::repository::manager::State =
            serde_json::from_slice(&fs::read(state_path)?)?;
        ensure!(
            matches!(state.schema, 2..=4),
            "Unknown legacy selector schema"
        );
        let target = state.target.clone().unwrap_or_else(|| {
            crate::repository::materialize::Target::Branch(state.branch.clone())
        });
        let identity = if matches!(target, crate::repository::materialize::Target::Branch(_)) {
            serde_json::to_vec(&(&state.repository, &state.branch))?
        } else {
            serde_json::to_vec(&(&state.repository, &target))?
        };
        ensure!(
            entry.file_name().to_str() == Some(blake3::hash(&identity).to_hex().as_str()),
            "Selector cache identity mismatch"
        );
        scoped_analysis(&entry.path().join("analysis"))?;
        let git = entry.path().join("git");
        let retired = entry.path().join("sigla-legacy-git");
        if directory(&retired)? {
            fs::remove_dir_all(&retired)?;
        }
        if directory(&git)? {
            for name in ["HEAD", "config"] {
                let m = fs::symlink_metadata(git.join(name))?;
                ensure!(
                    m.is_file() && !m.file_type().is_symlink(),
                    "Invalid legacy Git store"
                );
            }
            // This exact selector-owned location was Sigla's private object store.
            fs::rename(&git, &retired)?;
            fs::File::open(entry.path())?.sync_all()?;
            fs::remove_dir_all(retired)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn analysis(path: &Path, format: &[u8]) {
        fs::create_dir_all(path).unwrap();
        let env = unsafe {
            heed::EnvOpenOptions::new()
                .map_size(1024 * 1024)
                .max_dbs(7)
                .open(path)
        }
        .unwrap();
        let mut tx = env.write_txn().unwrap();
        let metadata = env
            .create_database::<heed::types::Str, heed::types::Bytes>(&mut tx, Some("metadata"))
            .unwrap();
        metadata.put(&mut tx, "format", format).unwrap();
        tx.commit().unwrap();
        env.prepare_for_closing().wait();
    }
    #[test]
    fn migration_removes_only_recognized_analysis_and_can_repeat() {
        let cache = tempfile::tempdir().unwrap();
        let old = cache.path().join("a".repeat(64));
        let unrelated = cache.path().join("b".repeat(64));
        let shared = cache.path().join("analysis");
        analysis(&old, b"sigla-facts-15");
        analysis(&unrelated, b"unrelated");
        fs::create_dir(&shared).unwrap();
        fs::write(shared.join("sentinel"), "preserve").unwrap();
        fs::create_dir(cache.path().join("packages")).unwrap();
        fs::write(cache.path().join("packages/source.cs"), "preserve").unwrap();
        migrate(cache.path()).unwrap();
        migrate(cache.path()).unwrap();
        assert!(!old.exists());
        assert!(unrelated.join("data.mdb").exists());
        assert!(shared.join("sentinel").exists());
        assert!(cache.path().join("packages/source.cs").exists());
    }
    #[test]
    fn migration_rejects_links_without_following_them() {
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        analysis(outside.path(), b"sigla-facts-15");
        std::os::unix::fs::symlink(outside.path(), cache.path().join("assemblies")).unwrap();
        assert!(migrate(cache.path()).is_err());
        assert!(outside.path().join("data.mdb").exists());
    }

    #[test]
    fn migration_resumes_partial_removal_and_preserves_materialized_sources() {
        let cache = tempfile::tempdir().unwrap();
        let old = cache.path().join("c".repeat(64));
        fs::create_dir(&old).unwrap();
        fs::write(old.join("sigla-legacy-analysis"), b"sigla-facts-15").unwrap();
        fs::write(old.join("lock.mdb"), b"leftover").unwrap();
        let repository = crate::repository::Repository::parse("https://github.com/fixture/repo")
            .unwrap()
            .unwrap();
        let key =
            blake3::hash(&serde_json::to_vec(&(&repository.identity, "main")).unwrap()).to_hex();
        let owner = cache.path().join("repositories").join(key.as_str());
        fs::create_dir_all(owner.join("source")).unwrap();
        fs::create_dir(owner.join("generated")).unwrap();
        fs::write(owner.join("source/Code.cs"), "class Kept {}").unwrap();
        fs::write(owner.join("generated/Generated.cs"), "class Generated {}").unwrap();
        let state = serde_json::json!({"schema":4,"repository":repository.identity,"transport":repository.transport,
            "branch":"main","last_use":0,"refreshed":0,"policy":"old","repair":false,"indexed_revision":null,
            "prepared":{"branch":"main","revision":"a".repeat(40),"selected":{},"tracked":{},"directories":[]}});
        fs::write(
            owner.join("state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let git = owner.join("git");
        assert!(
            std::process::Command::new("git")
                .args(["init", "--bare", "--quiet", "--template="])
                .arg(&git)
                .status()
                .unwrap()
                .success()
        );
        migrate(cache.path()).unwrap();
        assert!(!old.exists());
        assert!(!git.exists());
        assert!(owner.join("source/Code.cs").exists());
        assert!(owner.join("generated/Generated.cs").exists());
        fs::create_dir(owner.join("sigla-legacy-git")).unwrap();
        fs::write(owner.join("sigla-legacy-git/leftover"), "partial removal").unwrap();
        migrate(cache.path()).unwrap();
        assert!(!owner.join("sigla-legacy-git").exists());
    }
}
