//! Path normalization and relative path computation utilities.
//!
//! These replace the ad-hoc `pathdiff_relative()` and `paths_equal()` functions
//! that were scattered across the codebase. All path comparison and manipulation
//! should go through these functions.

use std::path::{Component, Path, PathBuf};

/// Normalize a path by resolving `.` and `..` components without following symlinks.
///
/// Unlike `std::fs::canonicalize()`, this works on non-existent paths and does not
/// touch the filesystem. It only resolves logical `.` and `..` components.
///
/// # Examples
/// ```
/// # use std::path::PathBuf;
/// # use zed_prj_workspace::paths::normalize_path;
/// assert_eq!(normalize_path(&PathBuf::from("/a/b/../c")), PathBuf::from("/a/c"));
/// assert_eq!(normalize_path(&PathBuf::from("/a/./b/./c")), PathBuf::from("/a/b/c"));
/// assert_eq!(normalize_path(&PathBuf::from("/a/b/../../c")), PathBuf::from("/c"));
/// ```
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {} // skip "."
            Component::ParentDir => {
                // Pop the last normal component if possible
                if let Some(Component::Normal(_)) = components.last() {
                    components.pop();
                } else {
                    // At root or no parent to pop — keep the ".." for relative paths
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }
    if components.is_empty() {
        PathBuf::from(".")
    } else {
        components.iter().collect()
    }
}

/// Compute a relative path from `base` to `target`.
///
/// Produces `..` segments as needed for sibling directories.
/// Both paths should be absolute for correct results.
///
/// # Examples
/// ```
/// # use std::path::{Path, PathBuf};
/// # use zed_prj_workspace::paths::relative_path;
/// assert_eq!(relative_path(Path::new("/a/b"), Path::new("/a/b/c/d")), PathBuf::from("c/d"));
/// assert_eq!(relative_path(Path::new("/a/b"), Path::new("/a/c")), PathBuf::from("../c"));
/// assert_eq!(relative_path(Path::new("/a/b/c"), Path::new("/a/d/e")), PathBuf::from("../../d/e"));
/// ```
pub fn relative_path(base: &Path, target: &Path) -> PathBuf {
    match pathdiff::diff_paths(target, base) {
        Some(rel) => {
            // pathdiff returns "" when base == target; normalize to "."
            if rel.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                rel
            }
        }
        None => {
            // Cannot compute relative path (e.g., different Windows drive letters)
            // Fall back to absolute target
            target.to_path_buf()
        }
    }
}

/// Compare two paths for equality after normalization.
///
/// This handles trailing slashes, `.` and `..` components.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    normalize_path(a) == normalize_path(b)
}

/// Parse Zed's workspace paths format (newline-separated plain text).
///
/// This is the canonical implementation — used by the shared library, hook, and MCP.
/// Previously duplicated in 3 places; now centralized here.
pub fn parse_workspace_paths(raw: &str) -> Vec<PathBuf> {
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Reconstruct user-visible ordered paths from Zed's DB columns.
///
/// Zed stores paths in lexicographic order (`paths` column) and a separate
/// permutation (`paths_order` column) where `order[lex_index] = user_position`.
///
/// `paths_order` is a comma-separated list like `"1,2,0,3,4"` — each value is
/// the user-visible position for the path at that lexicographic index.
///
/// If `paths_order` is empty or malformed, returns paths in original (lex) order.
pub fn reconstruct_ordered_paths(paths: &[PathBuf], paths_order_str: &str) -> Vec<PathBuf> {
    if paths_order_str.is_empty() {
        return paths.to_vec();
    }

    let order: Vec<usize> = paths_order_str
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    if order.len() != paths.len() {
        return paths.to_vec(); // malformed, fall back to lex order
    }

    // Validate all indices are in range
    if order.iter().any(|&i| i >= paths.len()) {
        return paths.to_vec();
    }

    // order[lex_index] = user_position
    // Invert: collect (user_position, path) pairs and sort by user_position
    let mut pairs: Vec<(usize, PathBuf)> = order
        .iter()
        .zip(paths.iter())
        .map(|(&user_pos, path)| (user_pos, path.clone()))
        .collect();
    pairs.sort_by_key(|(pos, _)| *pos);
    pairs.into_iter().map(|(_, path)| path).collect()
}

/// Serialize paths into Zed's `paths_order` format.
///
/// Zed format: `order[lex_index] = user_position`.
/// Given an ordered list of paths (user order) and the lex-sorted list,
/// produces the permutation string (e.g., `"1,2,0,3,4"`).
pub fn compute_paths_order(ordered: &[PathBuf], sorted: &[PathBuf]) -> String {
    // For each lex-path, find its position in the user-ordered list
    sorted
        .iter()
        .filter_map(|s| ordered.iter().position(|p| p == s))
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_path ---

    #[test]
    fn normalize_resolves_dotdot() {
        assert_eq!(normalize_path(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_resolves_dot() {
        assert_eq!(
            normalize_path(Path::new("/a/./b/./c")),
            PathBuf::from("/a/b/c")
        );
    }

    #[test]
    fn normalize_multiple_dotdot() {
        assert_eq!(
            normalize_path(Path::new("/a/b/c/../../d")),
            PathBuf::from("/a/d")
        );
    }

    #[test]
    fn normalize_already_clean() {
        assert_eq!(
            normalize_path(Path::new("/a/b/c")),
            PathBuf::from("/a/b/c")
        );
    }

    #[test]
    fn normalize_root_dotdot() {
        // Going above root: /a/../.. → root prefix + leftover ..
        // On Unix, Component::ParentDir stays when no normal component to pop
        // The RootDir component prevents further popping, so we get /..
        // In practice this path is invalid, but normalize handles it gracefully
        let result = normalize_path(Path::new("/a/../.."));
        // Either "/" or "/.." is acceptable — both represent an invalid upward traverse
        assert!(result == PathBuf::from("/") || result == PathBuf::from("/.."));
    }

    // --- relative_path ---

    #[test]
    fn relative_child() {
        assert_eq!(
            relative_path(Path::new("/a/b"), Path::new("/a/b/c/d")),
            PathBuf::from("c/d")
        );
    }

    #[test]
    fn relative_sibling() {
        assert_eq!(
            relative_path(Path::new("/a/b"), Path::new("/a/c")),
            PathBuf::from("../c")
        );
    }

    #[test]
    fn relative_same() {
        assert_eq!(
            relative_path(Path::new("/a/b"), Path::new("/a/b")),
            PathBuf::from(".")
        );
    }

    #[test]
    fn relative_deep_sibling() {
        assert_eq!(
            relative_path(Path::new("/a/b/c"), Path::new("/a/d/e")),
            PathBuf::from("../../d/e")
        );
    }

    // --- paths_equal ---

    #[test]
    fn equal_same_path() {
        assert!(paths_equal(Path::new("/a/b/c"), Path::new("/a/b/c")));
    }

    #[test]
    fn equal_with_dotdot() {
        assert!(paths_equal(
            Path::new("/a/b/../c"),
            Path::new("/a/c")
        ));
    }

    #[test]
    fn equal_with_dot() {
        assert!(paths_equal(Path::new("/a/./b"), Path::new("/a/b")));
    }

    #[test]
    fn not_equal_different() {
        assert!(!paths_equal(Path::new("/a/b"), Path::new("/a/c")));
    }

    // --- parse_workspace_paths ---

    #[test]
    fn parse_single() {
        let paths = parse_workspace_paths("/Users/test/project1");
        assert_eq!(paths, vec![PathBuf::from("/Users/test/project1")]);
    }

    #[test]
    fn parse_multi() {
        let paths = parse_workspace_paths("/Users/test/a\n/Users/test/b");
        assert_eq!(
            paths,
            vec![PathBuf::from("/Users/test/a"), PathBuf::from("/Users/test/b")]
        );
    }

    #[test]
    fn parse_empty() {
        assert!(parse_workspace_paths("").is_empty());
    }

    #[test]
    fn parse_with_whitespace() {
        let paths = parse_workspace_paths("  /a  \n  /b  \n\n  /c  ");
        assert_eq!(
            paths,
            vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")]
        );
    }

    // --- reconstruct_ordered_paths ---

    #[test]
    fn reconstruct_sequential_order() {
        let paths = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let ordered = reconstruct_ordered_paths(&paths, "0,1,2");
        assert_eq!(ordered, paths);
    }

    #[test]
    fn reconstruct_shuffled_order() {
        // order "2,0,1" means: lex_0→pos2, lex_1→pos0, lex_2→pos1
        // display: ["/b"(pos0), "/c"(pos1), "/a"(pos2)]
        let paths = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let ordered = reconstruct_ordered_paths(&paths, "2,0,1");
        assert_eq!(
            ordered,
            vec![PathBuf::from("/b"), PathBuf::from("/c"), PathBuf::from("/a")]
        );
    }

    #[test]
    fn reconstruct_empty_order_returns_lex() {
        let paths = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let ordered = reconstruct_ordered_paths(&paths, "");
        assert_eq!(ordered, paths);
    }

    #[test]
    fn reconstruct_malformed_order_returns_lex() {
        let paths = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let ordered = reconstruct_ordered_paths(&paths, "0,1,2"); // too many
        assert_eq!(ordered, paths);
    }

    #[test]
    fn reconstruct_real_example() {
        // From workspace 38: order "0,4,1,5,2,3"
        // order[lex_idx] = user_pos:
        //   lex_0(jdbox)→pos0, lex_1(cursor-jwt)→pos4, lex_2(websession)→pos1,
        //   lex_3(jsonwebtoken)→pos5, lex_4(dtformats)→pos2, lex_5(tidy-browser)→pos3
        let paths = vec![
            PathBuf::from("/codes/_my__jdbox"),
            PathBuf::from("/codes/_topic/_my__cursor-jwt-decoder"),
            PathBuf::from("/codes/_topic/websession-kimi"),
            PathBuf::from("/codes-repos/gh-keats__jsonwebtoken"),
            PathBuf::from("/codes-repos/gh-libyal__dtformats"),
            PathBuf::from("/codes-repos/gh-saying121__tidy-browser"),
        ];
        let ordered = reconstruct_ordered_paths(&paths, "0,4,1,5,2,3");
        assert_eq!(ordered[0], PathBuf::from("/codes/_my__jdbox"));          // pos 0
        assert_eq!(ordered[1], PathBuf::from("/codes/_topic/websession-kimi")); // pos 1
        assert_eq!(ordered[2], PathBuf::from("/codes-repos/gh-libyal__dtformats")); // pos 2
        assert_eq!(ordered[3], PathBuf::from("/codes-repos/gh-saying121__tidy-browser")); // pos 3
        assert_eq!(ordered[4], PathBuf::from("/codes/_topic/_my__cursor-jwt-decoder")); // pos 4
        assert_eq!(ordered[5], PathBuf::from("/codes-repos/gh-keats__jsonwebtoken")); // pos 5
    }

    // --- compute_paths_order ---

    #[test]
    fn compute_order_identity() {
        let paths = vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")];
        assert_eq!(compute_paths_order(&paths, &paths), "0,1,2");
    }

    #[test]
    fn compute_order_reversed() {
        let ordered = vec![PathBuf::from("/c"), PathBuf::from("/b"), PathBuf::from("/a")];
        let sorted = vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")];
        assert_eq!(compute_paths_order(&ordered, &sorted), "2,1,0");
    }

    #[test]
    fn compute_order_roundtrip() {
        let sorted = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
            PathBuf::from("/d"),
        ];
        let original_order = "2,0,3,1";
        let ordered = reconstruct_ordered_paths(&sorted, original_order);
        let computed = compute_paths_order(&ordered, &sorted);
        assert_eq!(computed, original_order);
    }
}
