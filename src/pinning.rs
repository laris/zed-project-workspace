//! Root pinning: ensure the target root (project home) stays at index 0.
//!
//! The "target root" is the folder whose `.zed/settings.json` defines `project_name`.
//! Zed reads settings from the first folder in the workspace, so the target root
//! MUST be at index 0 for `project_name` to be visible.
//!
//! Three-layer defense:
//! 1. DB hook: correct `paths_order` in Zed's SQLite DB after every write
//! 2. Zed auto: non-alphabetical DB order → `worktrees_reordered=true` → future adds append
//! 3. reuse_folders: fix current session's in-memory state

use crate::paths;
use std::path::{Path, PathBuf};

/// Determine the target root from a list of roots and a project_name.
///
/// The target root is the folder whose name matches the name portion of `project_name`.
/// For example, `project_name = "117:zed-project-workspace"` → target is the root
/// whose folder name is `"zed-project-workspace"`.
///
/// Fallback: if no name match, check which root contains `{name}.code-workspace`.
pub fn determine_target_root(roots: &[PathBuf], project_name: &str) -> Option<PathBuf> {
    let (_id, name) = crate::settings::parse_project_name(project_name);

    // Primary: match by folder name
    for root in roots {
        if let Some(folder_name) = root.file_name().and_then(|n| n.to_str()) {
            if folder_name == name {
                return Some(root.clone());
            }
        }
    }

    // Fallback: check which root contains {name}.code-workspace
    for root in roots {
        let ws_file = crate::settings::workspace_file_for_name(root, &name);
        if ws_file.exists() {
            return Some(root.clone());
        }
    }

    None
}

/// Check if the target root is at index 0. If not, return a reordered Vec.
///
/// The target root is moved to index 0; all other roots preserve their relative order.
/// Returns `None` if the target is already at index 0 (no reorder needed).
pub fn ensure_target_root_first(
    ordered_paths: &[PathBuf],
    target_root: &Path,
) -> Option<Vec<PathBuf>> {
    if ordered_paths.is_empty() {
        return None;
    }

    // Check if already at index 0
    if paths::paths_equal(&ordered_paths[0], target_root) {
        return None;
    }

    // Find the target in the list
    let target_idx = ordered_paths
        .iter()
        .position(|p| paths::paths_equal(p, target_root))?;

    // Build reordered list: target first, then others in original relative order
    let mut reordered = Vec::with_capacity(ordered_paths.len());
    reordered.push(ordered_paths[target_idx].clone());
    for (i, path) in ordered_paths.iter().enumerate() {
        if i != target_idx {
            reordered.push(path.clone());
        }
    }

    Some(reordered)
}

/// Pin the target root at index 0 by calling `reuse_folders` if needed.
///
/// Returns `Ok(true)` if reorder was performed, `Ok(false)` if already correct.
pub fn pin_target_root(
    ordered_paths: &[PathBuf],
    target_root: &Path,
    channel: Option<&str>,
) -> Result<bool, String> {
    match ensure_target_root_first(ordered_paths, target_root) {
        None => Ok(false), // Already correct
        Some(reordered) => {
            tracing::info!(
                "Pinning target root at index 0: {}",
                target_root.display()
            );
            crate::hook_client::invoke_zed_reuse(&reordered, channel)?;
            Ok(true)
        }
    }
}

/// Correct `paths_order` in the database to ensure target root is at display index 0.
///
/// Zed's `paths_order` format: `order[lex_index] = user_position`.
/// Each value is the display position for the path at that lexicographic index.
///
/// Example: paths=["/a", "/b", "/target"], order="1,2,0"
///   → lex_0("/a") at pos 1, lex_1("/b") at pos 2, lex_2("/target") at pos 0
///   → display: ["/target", "/a", "/b"]
///
/// Returns `Some(new_order_string)` if correction needed, `None` if already correct.
pub fn correct_paths_order(
    lex_paths: &[PathBuf],
    current_order: &str,
    target_root: &Path,
) -> Option<String> {
    if lex_paths.is_empty() {
        return None;
    }

    // Parse current order: order[lex_index] = user_position
    let order: Vec<usize> = if current_order.is_empty() {
        (0..lex_paths.len()).collect()
    } else {
        current_order
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .collect()
    };

    if order.len() != lex_paths.len() {
        tracing::warn!(
            "paths_order length mismatch: {} order entries vs {} paths",
            order.len(),
            lex_paths.len()
        );
        return None;
    }

    // Reconstruct display order: collect (lex_idx, user_pos, path) and sort by user_pos
    let mut display: Vec<(usize, usize, &PathBuf)> = order
        .iter()
        .enumerate()
        .map(|(lex_idx, &user_pos)| (lex_idx, user_pos, &lex_paths[lex_idx]))
        .collect();
    display.sort_by_key(|&(_, user_pos, _)| user_pos);

    // Check if target is already at display position 0
    if paths::paths_equal(display[0].2, target_root) {
        return None;
    }

    // Find target in display order
    let target_display_idx = display
        .iter()
        .position(|(_, _, p)| paths::paths_equal(p, target_root))?;

    // Build new display order: target first, then others preserving relative order
    let mut new_display_order: Vec<usize> = Vec::with_capacity(display.len());
    new_display_order.push(display[target_display_idx].0); // target's lex_idx
    for (i, &(lex_idx, _, _)) in display.iter().enumerate() {
        if i != target_display_idx {
            new_display_order.push(lex_idx);
        }
    }

    // Convert to paths_order format: order[lex_index] = new_user_position
    let mut new_order = vec![0usize; lex_paths.len()];
    for (new_user_pos, &lex_idx) in new_display_order.iter().enumerate() {
        new_order[lex_idx] = new_user_pos;
    }

    let new_order_str = new_order
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");

    Some(new_order_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determine_target_by_folder_name() {
        let roots = vec![
            PathBuf::from("/codes/dylib-kit"),
            PathBuf::from("/codes/zed-project-workspace"),
            PathBuf::from("/codes/zed-yolo-hook"),
        ];
        let target = determine_target_root(&roots, "117:zed-project-workspace");
        assert_eq!(
            target,
            Some(PathBuf::from("/codes/zed-project-workspace"))
        );
    }

    #[test]
    fn determine_target_no_match() {
        let roots = vec![PathBuf::from("/codes/foo"), PathBuf::from("/codes/bar")];
        let target = determine_target_root(&roots, "117:nonexistent");
        assert_eq!(target, None);
    }

    #[test]
    fn ensure_first_already_correct() {
        let paths = vec![
            PathBuf::from("/codes/target"),
            PathBuf::from("/codes/other"),
        ];
        assert_eq!(
            ensure_target_root_first(&paths, Path::new("/codes/target")),
            None
        );
    }

    #[test]
    fn ensure_first_needs_reorder() {
        let paths = vec![
            PathBuf::from("/codes/alpha"),
            PathBuf::from("/codes/target"),
            PathBuf::from("/codes/zeta"),
        ];
        let result = ensure_target_root_first(&paths, Path::new("/codes/target"));
        assert_eq!(
            result,
            Some(vec![
                PathBuf::from("/codes/target"),
                PathBuf::from("/codes/alpha"),
                PathBuf::from("/codes/zeta"),
            ])
        );
    }

    #[test]
    fn ensure_first_preserves_relative_order() {
        let paths = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/target"),
            PathBuf::from("/c"),
            PathBuf::from("/d"),
        ];
        let result = ensure_target_root_first(&paths, Path::new("/target")).unwrap();
        assert_eq!(result[0], PathBuf::from("/target"));
        assert_eq!(result[1], PathBuf::from("/a"));
        assert_eq!(result[2], PathBuf::from("/b"));
        assert_eq!(result[3], PathBuf::from("/c"));
        assert_eq!(result[4], PathBuf::from("/d"));
    }

    #[test]
    fn ensure_first_target_not_in_list() {
        let paths = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert_eq!(
            ensure_target_root_first(&paths, Path::new("/missing")),
            None
        );
    }

    #[test]
    fn correct_order_already_correct() {
        // lex_paths: ["/a", "/b", "/target"]
        // order: "1,2,0" → lex_0 at pos 1, lex_1 at pos 2, lex_2 at pos 0
        // display: ["/target", "/a", "/b"]
        let lex_paths = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/target"),
        ];
        let result = correct_paths_order(&lex_paths, "1,2,0", Path::new("/target"));
        assert_eq!(result, None);
    }

    #[test]
    fn correct_order_needs_fix() {
        // lex_paths: ["/a", "/b", "/target"]
        // order: "0,1,2" → identity → display: ["/a", "/b", "/target"]
        // should become: "1,2,0" → display: ["/target", "/a", "/b"]
        let lex_paths = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/target"),
        ];
        let result = correct_paths_order(&lex_paths, "0,1,2", Path::new("/target"));
        assert_eq!(result, Some("1,2,0".to_string()));
    }

    #[test]
    fn correct_order_empty() {
        let result = correct_paths_order(&[], "", Path::new("/target"));
        assert_eq!(result, None);
    }

    #[test]
    fn correct_order_no_explicit_order() {
        // No paths_order → identity permutation → lex order is display order
        // lex_paths: ["/a", "/target", "/z"]
        // identity: "0,1,2" → display: ["/a", "/target", "/z"]
        // should become: "1,0,2" → lex_0 at pos 1, lex_1("/target") at pos 0, lex_2 at pos 2
        // display: ["/target", "/a", "/z"]
        let lex_paths = vec![
            PathBuf::from("/a"),
            PathBuf::from("/target"),
            PathBuf::from("/z"),
        ];
        let result = correct_paths_order(&lex_paths, "", Path::new("/target"));
        assert_eq!(result, Some("1,0,2".to_string()));
    }
}
