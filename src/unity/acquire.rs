pub(crate) use crate::metadata_archive::editor;
pub use crate::metadata_archive::prefetch;

use anyhow::Result;
use std::{fs, path::Path};

pub fn inspect_archive(
    archive: &Path,
    destination: &Path,
    version: super::UnityVersion,
) -> Result<Vec<String>> {
    fs::create_dir_all(destination)?;
    let inventory = crate::acquisition::extract(fs::File::open(archive)?, destination, |path| {
        let Ok(path) = path.strip_prefix("Editor/Data") else {
            return false;
        };
        if path.starts_with("Resources/PackageManager/BuiltInPackages") {
            return crate::acquisition::analysis_input(path);
        }
        path == Path::new("Resources/modules.asset") || path.extension().is_some_and(|e| e == "dll")
    })?;
    super::catalog::validate_references(&destination.join("Editor/Data"), version)?;
    Ok(inventory)
}
