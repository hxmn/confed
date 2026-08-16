//! Page title → filesystem path. The filename is a convenience, never an
//! identity: `page_id` in the frontmatter is the stable key, so renames on
//! either side are safe.

use std::collections::HashMap;
use unicode_normalization::UnicodeNormalization;

/// Longest slug in bytes. Keeps full paths comfortably inside the ~255-byte
/// component limit on every filesystem confed targets, with room for the
/// collision suffix and the `.md` extension.
pub const MAX_SLUG_BYTES: usize = 120;

const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Names Windows refuses regardless of extension.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Turn a page title into a filesystem-safe component (no extension).
pub fn slugify(title: &str) -> String {
    let normalized: String = title.nfc().collect();

    let mut out = String::with_capacity(normalized.len());
    let mut pending_space = false;
    for ch in normalized.chars() {
        // Whitespace is tested first: newlines and tabs are also control
        // characters, and they should collapse into a space rather than a dash.
        let mapped = if ch.is_whitespace() {
            pending_space = true;
            continue;
        } else if ILLEGAL.contains(&ch) || ch.is_control() {
            '-'
        } else {
            ch
        };
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(mapped);
    }

    // A leading dot would hide the file (and collide with the sidecar convention);
    // trailing dots and spaces are invalid on Windows.
    let mut slug = out.trim().trim_start_matches('.').trim().to_string();
    while slug.ends_with('.') || slug.ends_with(' ') {
        slug.pop();
    }

    slug = truncate_bytes(&slug, MAX_SLUG_BYTES).trim_end().to_string();

    if slug.is_empty() {
        return "untitled".to_string();
    }
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&slug)) {
        return format!("{slug}-page");
    }
    slug
}

/// Truncate at a UTF-8 character boundary.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Disambiguate two sibling pages whose titles slug to the same string.
/// Deterministic in the page id, so it survives re-pulls.
pub fn disambiguate(slug: &str, page_id: &str) -> String {
    let tail: String = page_id.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    let room = MAX_SLUG_BYTES.saturating_sub(tail.len() + 1);
    format!("{}~{}", truncate_bytes(slug, room).trim_end(), tail)
}

/// Assigns collision-free slugs to a set of siblings.
///
/// The first page (in the caller's order — confed uses server position, then
/// title, then id) keeps the plain slug; later ones get the `~id` suffix.
#[derive(Debug, Default)]
pub struct SlugAllocator {
    /// Lowercased slug → page id that owns it. Case-insensitive because macOS
    /// and Windows filesystems are.
    taken: HashMap<String, String>,
}

impl SlugAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a slug for `page_id`, returning the name to actually use.
    pub fn allocate(&mut self, title: &str, page_id: &str) -> String {
        let base = slugify(title);
        let key = base.to_lowercase();
        match self.taken.get(&key) {
            None => {
                self.taken.insert(key, page_id.to_string());
                base
            }
            Some(owner) if owner == page_id => base,
            Some(_) => {
                let disambiguated = disambiguate(&base, page_id);
                self.taken.insert(disambiguated.to_lowercase(), page_id.to_string());
                disambiguated
            }
        }
    }

    /// Pre-reserve a name that already exists on disk (so a re-pull keeps it).
    pub fn reserve(&mut self, slug: &str, page_id: &str) {
        self.taken.insert(slug.to_lowercase(), page_id.to_string());
    }
}

/// The hidden sidecar directory that holds a page's attachments and comments:
/// `Onboarding.md` → `.Onboarding`.
pub fn sidecar_dir_name(slug: &str) -> String {
    format!(".{slug}")
}

/// Build the Markdown path for a page from its ancestors' slugs.
///
/// `ancestors` runs root-first and excludes the page itself. A page's children
/// live in a directory named after its slug, next to its `.md` file.
pub fn page_path(ancestors: &[String], slug: &str) -> String {
    let mut parts: Vec<&str> = ancestors.iter().map(String::as_str).collect();
    let file = format!("{slug}.md");
    parts.push(&file);
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn illegal_characters_become_dashes() {
        assert_eq!(slugify("Q3/Q4 Plan"), "Q3-Q4 Plan");
        assert_eq!(slugify("What? Why: How"), "What- Why- How");
        assert_eq!(slugify("a\u{0}b"), "a-b");
    }

    #[test]
    fn whitespace_is_collapsed_and_trimmed() {
        assert_eq!(slugify("  Team   Handbook \n"), "Team Handbook");
        assert_eq!(slugify("Tab\tSeparated"), "Tab Separated");
    }

    #[test]
    fn hidden_and_windows_hostile_names_are_fixed() {
        assert_eq!(slugify(".hidden"), "hidden");
        assert_eq!(slugify("...notes"), "notes");
        assert_eq!(slugify("trailing dots..."), "trailing dots");
        assert_eq!(slugify("trailing space "), "trailing space");
        assert_eq!(slugify("CON"), "CON-page");
        assert_eq!(slugify("nul"), "nul-page");
    }

    #[test]
    fn empty_titles_get_a_placeholder() {
        assert_eq!(slugify(""), "untitled");
        assert_eq!(slugify("   "), "untitled");
        assert_eq!(slugify("..."), "untitled");
    }

    #[test]
    fn unicode_survives_and_stays_on_char_boundaries() {
        assert_eq!(slugify("Café Menü"), "Café Menü");
        assert_eq!(slugify("日本語のページ"), "日本語のページ");
        assert_eq!(slugify("emoji 🚀 page"), "emoji 🚀 page");

        let long = "é".repeat(200);
        let slug = slugify(&long);
        assert!(slug.len() <= MAX_SLUG_BYTES);
        assert!(std::str::from_utf8(slug.as_bytes()).is_ok());
    }

    #[test]
    fn collisions_are_resolved_deterministically() {
        let mut alloc = SlugAllocator::new();
        assert_eq!(alloc.allocate("Plan", "163841"), "Plan");
        assert_eq!(alloc.allocate("Plan", "163842"), "Plan~163842");
        // Same page asking again keeps its name.
        assert_eq!(alloc.allocate("Plan", "163841"), "Plan");

        let mut again = SlugAllocator::new();
        assert_eq!(again.allocate("Plan", "163841"), "Plan");
        assert_eq!(again.allocate("Plan", "163842"), "Plan~163842");
    }

    #[test]
    fn collisions_are_case_insensitive() {
        let mut alloc = SlugAllocator::new();
        assert_eq!(alloc.allocate("Plan", "1"), "Plan");
        let second = alloc.allocate("PLAN", "2");
        assert_ne!(second.to_lowercase(), "plan", "macOS and Windows would collide");
    }

    #[test]
    fn disambiguated_slugs_still_respect_the_length_cap() {
        let long = "x".repeat(300);
        let slug = disambiguate(&slugify(&long), "1234567890");
        assert!(slug.len() <= MAX_SLUG_BYTES, "{} bytes", slug.len());
        assert!(slug.ends_with("~567890"));
    }

    #[test]
    fn paths_nest_children_under_a_directory_named_for_the_parent() {
        assert_eq!(page_path(&[], "Team Handbook"), "Team Handbook.md");
        assert_eq!(
            page_path(&["Team Handbook".into()], "Onboarding"),
            "Team Handbook/Onboarding.md"
        );
        assert_eq!(sidecar_dir_name("Onboarding"), ".Onboarding");
    }

    #[test]
    fn slugs_never_contain_path_separators() {
        for title in ["a/b", "a\\b", "../../etc/passwd", "C:\\Windows"] {
            let slug = slugify(title);
            assert!(!slug.contains('/'), "{slug}");
            assert!(!slug.contains('\\'), "{slug}");
            assert!(!slug.starts_with('.'), "{slug}");
        }
    }
}
