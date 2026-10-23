//! The daemon's stores of images, layers and mirrors: directories by name.

use std::fs;
use std::path::Path;

use anyhow::bail;

use super::valid_name;

/// The names in dir, sorted, that are visible and pass keep.
pub fn list(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| keep(&e.path()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Removes dir/name.
pub fn remove(dir: &Path, name: &str) -> anyhow::Result<()> {
    if !valid_name(name) {
        bail!("bad name {name}");
    }
    let p = dir.join(name);
    if !p.exists() {
        bail!("no {name}");
    }
    fs::remove_dir_all(p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_skip_hidden_and_unwanted_entries() {
        let d = tempfile::tempdir().unwrap();
        for n in ["b", "a", ".partial"] {
            fs::create_dir(d.path().join(n)).unwrap();
        }
        fs::write(d.path().join("c"), "").unwrap();
        assert_eq!(list(d.path(), |p| p.is_dir()), ["a", "b"]);
    }

    #[test]
    fn removal_takes_names_not_paths() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("a")).unwrap();
        assert!(remove(d.path(), "../x").is_err());
        assert!(remove(d.path(), "missing").is_err());
        remove(d.path(), "a").unwrap();
        assert!(!d.path().join("a").exists());
    }
}
