//! Exact-root validation shared by destructive cache collectors.

use std::fs::Metadata;
use std::io;
use std::path::{Component, Path};

/// True for Unix/Windows symbolic links and Windows reparse points such as
/// directory junctions.  Destructive collectors must not follow any of them.
pub fn is_link_or_reparse(metadata: &Metadata) -> bool {
    crate::platform::fs::links::is_link_or_reparse(metadata)
}

/// Validate that `directory` is a real directory beneath the exact selected
/// product `boundary`, with no link/reparse component between them.
pub fn validate_owned_directory(boundary: &Path, directory: &Path) -> io::Result<()> {
    let relative = directory.strip_prefix(boundary).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "destructive root {} escapes selected product root {}",
                directory.display(),
                boundary.display()
            ),
        )
    })?;
    validate_real_directory(boundary)?;
    let mut current = boundary.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unsafe destructive path component in {}",
                    directory.display()
                ),
            ));
        };
        current.push(component);
        validate_real_directory(&current)?;
    }
    let canonical_boundary = std::fs::canonicalize(boundary)?;
    let canonical_directory = std::fs::canonicalize(directory)?;
    if !canonical_directory.starts_with(&canonical_boundary) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "resolved destructive root {} escapes selected product root {}",
                canonical_directory.display(),
                canonical_boundary.display()
            ),
        ));
    }
    Ok(())
}

fn validate_real_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a real directory", path.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(owned_directory_rejects_linked_collection_root, {
        let temp = tempfile::tempdir().unwrap();
        let boundary = temp.path().join("owned");
        let external = temp.path().join("external");
        std::fs::create_dir_all(&boundary).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let linked = boundary.join("linked");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, &linked).unwrap();
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(&linked)
                .arg(&external)
                .status()
                .unwrap();
            assert!(status.success(), "create test junction");
        }
        assert!(validate_owned_directory(&boundary, &linked).is_err());
    });
}
