//! create an isolated manifest copy with read-only access to the original source tree.
//! this is a benchmark fixture, not a Git worktree or an editable Unity project.
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let source = PathBuf::from(args.next().context("Expected source workspace")?).canonicalize()?;
    let target = PathBuf::from(args.next().context("Expected new overlay directory")?);
    ensure!(!target.exists(), "Overlay directory already exists");
    std::fs::create_dir_all(&target)?;
    for entry in std::fs::read_dir(&source)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let destination = target.join(entry.file_name());
        if path
            .extension()
            .is_some_and(|extension| extension == "csproj" || extension == "sln")
        {
            std::fs::copy(path, destination)?;
        } else {
            std::os::unix::fs::symlink(path, destination)?;
        }
    }
    let policy =
        sigla::discovery::Policy::new(vec![source.parent().unwrap().into(), target.clone()])?;
    let original = sigla::discovery::discover(&source, &policy)?;
    let overlay = sigla::discovery::discover(&target, &policy)?;
    ensure!(
        original.projects.len() == overlay.projects.len()
            && original.sources.len() == overlay.sources.len(),
        "Overlay membership differs from its source workspace"
    );
    println!(
        "{} projects, {} source memberships: {}",
        overlay.projects.len(),
        overlay.sources.len(),
        target.display()
    );
    Ok(())
}
