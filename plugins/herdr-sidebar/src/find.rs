//! Project-wide name search behind the Explorer's `/` find: every file or
//! folder under the root whose NAME contains the query. UI-free so it is
//! unit-testable; `explorer_app.rs` reveals and highlights the result.
//!
//! It finds what the tree SHOWS. The tree lists gitignored entries (dimmed),
//! so a search that honours `.gitignore` outright — fd, or the plain `ignore`
//! walker — could never find `target/` or `node_modules/` sitting right
//! there. So the walk honours `.gitignore` for where it DESCENDS, and in every
//! folder it visits also matches the names of the ignored entries directly
//! inside it, without going into them: `/target` finds the folder, not the
//! thousands of build files under it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Paths whose name contains `query`, plus whether the list was cut at
/// `limit`. Smart case: case-insensitive unless the query has an uppercase
/// letter. `.git` is never searched.
pub fn find_paths(
    root: &Path,
    query: &str,
    show_hidden: bool,
    limit: usize,
) -> (Vec<PathBuf>, bool) {
    let query = query.trim();
    if query.is_empty() {
        return (Vec::new(), false);
    }
    let case_sensitive = query.chars().any(char::is_uppercase);
    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let matches = |name: &str| {
        if case_sensitive {
            name.contains(&needle)
        } else {
            name.to_lowercase().contains(&needle)
        }
    };

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!show_hidden)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    'walk: for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        // Every child of a folder the walk reaches — ignored or not — is a
        // candidate; only non-ignored folders are walked into.
        let Ok(children) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for child in children.flatten() {
            let name = child.file_name().to_string_lossy().into_owned();
            if name == ".git" || (!show_hidden && name.starts_with('.')) || !matches(&name) {
                continue;
            }
            let path = child.path();
            if seen.insert(path.clone()) {
                found.push(path);
                if found.len() > limit {
                    break 'walk;
                }
            }
        }
    }
    let truncated = found.len() > limit;
    found.truncate(limit);
    (found, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("hs-find-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for d in ["src/nested", ".git", "target/debug"] {
                std::fs::create_dir_all(dir.join(d)).unwrap();
            }
            for file in [
                "src/App.rs",
                "src/nested/app_test.rs",
                "README.md",
                ".git/app",
                "target/debug/app_bin",
            ] {
                std::fs::write(dir.join(file), "").unwrap();
            }
            std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
            Self(dir)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        let mut names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn smart_case_files_and_folders_and_never_git() {
        let tree = TempTree::new("case");
        assert_eq!(
            names(&find_paths(&tree.0, "app", true, 10).0),
            ["App.rs", "app_test.rs"]
        );
        assert_eq!(names(&find_paths(&tree.0, "App", true, 10).0), ["App.rs"]);
        assert_eq!(names(&find_paths(&tree.0, "nest", true, 10).0), ["nested"]);
    }

    #[test]
    fn an_ignored_folder_is_found_but_not_searched_inside() {
        let tree = TempTree::new("ignored");
        assert_eq!(
            names(&find_paths(&tree.0, "target", true, 10).0),
            ["target"]
        );
        // app_bin lives under the ignored target/: never walked into.
        assert!(!names(&find_paths(&tree.0, "app_bin", true, 10).0).contains(&"app_bin".into()));
    }

    #[test]
    fn results_are_capped_and_flagged() {
        let tree = TempTree::new("cap");
        let (found, truncated) = find_paths(&tree.0, "app", true, 1);
        assert_eq!(found.len(), 1);
        assert!(truncated);
        let (found, truncated) = find_paths(&tree.0, "app", true, 10);
        assert_eq!(found.len(), 2);
        assert!(!truncated);
    }

    #[test]
    fn blank_query_finds_nothing() {
        let tree = TempTree::new("blank");
        assert_eq!(find_paths(&tree.0, "  ", true, 10), (Vec::new(), false));
    }
}
