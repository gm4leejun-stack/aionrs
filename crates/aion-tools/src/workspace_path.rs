use std::fs;
use std::path::{Component, Path};

/// Returns whether `candidate` resolves inside `workspace`.
///
/// The candidate may not exist yet (Write creates new files), so only the
/// nearest existing ancestor is canonicalized. Existing symlinks and `..`
/// components are resolved before the containment check.
pub fn is_within_workspace(workspace: &Path, candidate: &Path) -> bool {
    if !candidate.is_absolute() {
        return false;
    }

    let Ok(workspace) = fs::canonicalize(workspace) else {
        return false;
    };

    let mut existing = candidate.to_path_buf();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|name| name.to_os_string()) else {
            return false;
        };
        suffix.push(name);
        if !existing.pop() {
            return false;
        }
    }

    let Ok(existing) = fs::canonicalize(existing) else {
        return false;
    };
    let mut resolved = existing;
    for component in suffix.iter().rev() {
        match Path::new(component).components().next() {
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            Some(Component::Normal(name)) => resolved.push(name),
            _ => return false,
        }
    }

    resolved.starts_with(workspace)
}

#[cfg(test)]
mod tests {
    use super::is_within_workspace;
    use tempfile::tempdir;

    #[test]
    fn rejects_parent_traversal() {
        let workspace = tempdir().unwrap();
        assert!(!is_within_workspace(
            workspace.path(),
            &workspace.path().join("..").join("outside.txt"),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_to_outside() {
        let workspace = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("link")).unwrap();
        assert!(!is_within_workspace(
            workspace.path(),
            &workspace.path().join("link/file.txt"),
        ));
    }

    #[test]
    fn allows_new_file_inside_workspace() {
        let workspace = tempdir().unwrap();
        assert!(is_within_workspace(
            workspace.path(),
            &workspace.path().join("nested/file.txt"),
        ));
    }
}
