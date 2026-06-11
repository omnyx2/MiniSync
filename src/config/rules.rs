//! Glob pattern matching for sync rules.
//!
//! Supports standard glob patterns: `*`, `**`, `?`, `[...]`.

use glob::Pattern;

/// Check if a relative path matches a glob pattern.
///
/// The pattern is matched against the full relative path.
/// Patterns like `*.pdf` match filenames; `src/**` matches directory trees.
pub fn glob_match(pattern: &str, rel_path: &str) -> bool {
    // Try matching as-is first
    if let Ok(pat) = Pattern::new(pattern) {
        if pat.matches(rel_path) {
            return true;
        }
    }

    // For patterns like "*.pdf", also try matching just the filename
    if !pattern.contains('/') {
        if let Some(filename) = std::path::Path::new(rel_path).file_name() {
            if let Ok(pat) = Pattern::new(pattern) {
                if pat.matches(&filename.to_string_lossy()) {
                    return true;
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_extension() {
        assert!(glob_match("*.pdf", "report.pdf"));
        assert!(glob_match("*.pdf", "docs/report.pdf"));
        assert!(!glob_match("*.pdf", "report.txt"));
    }

    #[test]
    fn directory_glob() {
        assert!(glob_match("src/**", "src/main.rs"));
        assert!(glob_match("src/**", "src/lib/utils.rs"));
        assert!(!glob_match("src/**", "test/main.rs"));
    }

    #[test]
    fn question_mark() {
        assert!(glob_match("file?.txt", "file1.txt"));
        assert!(!glob_match("file?.txt", "file12.txt"));
    }

    #[test]
    fn exact_filename() {
        assert!(glob_match("Makefile", "Makefile"));
        assert!(!glob_match("Makefile", "NotMakefile"));
    }
}
