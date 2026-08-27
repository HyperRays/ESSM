//! Render-time redaction for shareable screenshots and recordings.
//!
//! When enabled, personally identifying strings — the account's username
//! as found in `$USER` and the home directory's final component — are
//! replaced with `user` wherever they appear in rendered names and
//! paths, case-insensitively. Ordinary file and directory names stay
//! real. Only presentation changes: sizes, counts, and the real paths
//! behind click actions (Reveal in Finder, filtering) are untouched.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

const PLACEHOLDER: &str = "user";

static ANONYMIZED: AtomicBool = AtomicBool::new(false);
static REDACTIONS: OnceLock<Vec<String>> = OnceLock::new();

pub fn is_anonymized() -> bool {
    ANONYMIZED.load(Ordering::Relaxed)
}

pub fn set_anonymized(anonymized: bool) {
    ANONYMIZED.store(anonymized, Ordering::Relaxed);
}

/// One file or directory name for display.
pub fn name(real: &str) -> String {
    if is_anonymized() {
        redact(real, redactions())
    } else {
        real.to_owned()
    }
}

/// A full path for display; redaction is substring-based, so this is
/// the same operation.
pub fn path(real: &str) -> String {
    name(real)
}

/// The private terms to hide, longest first so an account name that
/// contains another term is replaced as a whole.
fn redactions() -> &'static [String] {
    REDACTIONS.get_or_init(|| {
        let mut terms: Vec<String> = Vec::new();
        for candidate in [
            std::env::var("USER").ok(),
            std::env::var("HOME")
                .ok()
                .and_then(|home| home.rsplit('/').next().map(str::to_owned)),
        ]
        .into_iter()
        .flatten()
        {
            if candidate.len() >= 3
                && !candidate.eq_ignore_ascii_case(PLACEHOLDER)
                && !terms
                    .iter()
                    .any(|term| term.eq_ignore_ascii_case(&candidate))
            {
                terms.push(candidate);
            }
        }
        terms.sort_by_key(|term| std::cmp::Reverse(term.len()));
        terms
    })
}

/// Replaces every case-insensitive occurrence of each term.
fn redact(text: &str, terms: &[String]) -> String {
    let mut redacted = text.to_owned();
    for term in terms {
        redacted = replace_ignore_ascii_case(&redacted, term);
    }
    redacted
}

fn replace_ignore_ascii_case(text: &str, term: &str) -> String {
    let text_bytes = text.as_bytes();
    let term_bytes = term.as_bytes();
    if term_bytes.is_empty() || text_bytes.len() < term_bytes.len() {
        return text.to_owned();
    }

    let mut output = String::with_capacity(text.len());
    let mut position = 0;
    while position < text_bytes.len() {
        let end = position + term_bytes.len();
        // Match only on character boundaries so multi-byte names are
        // never split mid-character.
        if end <= text_bytes.len()
            && text.is_char_boundary(position)
            && text.is_char_boundary(end)
            && text_bytes[position..end].eq_ignore_ascii_case(term_bytes)
        {
            output.push_str(PLACEHOLDER);
            position = end;
        } else {
            let character = text[position..].chars().next().expect("in-bounds char");
            output.push(character);
            position += character.len_utf8();
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hides_private_terms_and_keeps_ordinary_names() {
        let terms = vec!["soham".to_owned()];
        assert_eq!(
            redact("/Users/soham/Documents/programming", &terms),
            "/Users/user/Documents/programming"
        );
        assert_eq!(
            redact("/Users/Soham/SOHAMs-backup", &terms),
            "/Users/user/users-backup"
        );
        assert_eq!(
            redact("/tmp/project/cache.png", &terms),
            "/tmp/project/cache.png"
        );

        set_anonymized(false);
        assert_eq!(name("/Users/whoever/file.txt"), "/Users/whoever/file.txt");
    }
}
