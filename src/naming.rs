//! What an agent name may be.
//!
//! An agent's name is not only a label: it is the single path component that every
//! one of its directories is built from (its configuration, its workspace, its run
//! records). The rule below is therefore deliberately narrow, so that any name that
//! passes it can only ever name one directory directly under the directory meant for
//! it — never another agent's directory, and never a path that climbs out.
//!
//! A name is:
//!
//! - non-empty, and at most [`MAX_AGENT_NAME_LEN`] ASCII characters;
//! - lowercase ASCII letters, ASCII digits and hyphens only;
//! - neither starting nor ending with a hyphen, and without a doubled hyphen;
//! - all of it, because a name of `.`, `..` or `-` would name something else.
//!
//! Distinct names always give distinct directories: a valid name is a single path
//! component that holds no separator, so no two of them can nest or collide. The
//! tests below assert that over a sample rather than leave it to inspection.

/// The longest an agent name may be.
pub const MAX_AGENT_NAME_LEN: usize = 32;

/// What is wrong with a name, if anything.
///
/// `None` means the name is acceptable. `Some(reason)` is a phrase that completes the
/// sentence "this name <reason>", so a caller can hand it straight to the user.
pub fn agent_name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("is empty; it needs a name".to_owned());
    }
    if name.chars().count() > MAX_AGENT_NAME_LEN {
        return Some(format!("is longer than {MAX_AGENT_NAME_LEN} characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Some(
            "is not allowed; a name uses lowercase letters, digits and hyphens only".to_owned(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Some("starts or ends with a hyphen, which is not allowed".to_owned());
    }
    if name.contains("--") {
        return Some("holds a doubled hyphen, which is not allowed".to_owned());
    }
    None
}

/// Whether a name may be turned into a directory.
pub fn is_safe_agent_name(name: &str) -> bool {
    agent_name_problem(name).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_are_accepted() {
        for name in ["a", "agent-1", "scribe", "checker-2", "x9-y8-z7"] {
            assert!(
                is_safe_agent_name(name),
                "{name} must be accepted: {:?}",
                agent_name_problem(name)
            );
        }
    }

    #[test]
    fn a_name_that_would_escape_its_directory_is_refused() {
        for name in ["..", ".", "-", "a/b", "a\\b", "../x", "a b", "a.b"] {
            assert!(!is_safe_agent_name(name), "{name} must be refused");
        }
    }

    #[test]
    fn a_name_with_a_character_that_is_not_lowercase_digit_or_hyphen_is_refused() {
        for name in ["Scribe", "scribé", "a_b", "a:b", "a*b", "a\nb"] {
            assert!(!is_safe_agent_name(name), "{name} must be refused");
        }
    }

    #[test]
    fn a_hyphen_at_an_end_or_doubled_is_refused() {
        assert!(
            agent_name_problem("-a")
                .expect("refused")
                .contains("hyphen")
        );
        assert!(
            agent_name_problem("a-")
                .expect("refused")
                .contains("hyphen")
        );
        assert!(
            agent_name_problem("a--b")
                .expect("refused")
                .contains("doubled hyphen")
        );
    }

    #[test]
    fn an_empty_name_and_a_name_that_is_too_long_are_refused() {
        assert!(agent_name_problem("").expect("refused").contains("empty"));
        let long = "a".repeat(MAX_AGENT_NAME_LEN + 1);
        assert!(
            agent_name_problem(&long)
                .expect("refused")
                .contains("longer than")
        );
        let at_the_bound = "a".repeat(MAX_AGENT_NAME_LEN);
        assert!(is_safe_agent_name(&at_the_bound));
    }

    #[test]
    fn every_accepted_name_is_one_directory_component() {
        // The property the whole module exists for: a valid name holds no separator,
        // is not a dot name, and therefore names exactly one directory.
        for name in ["a", "agent-1", "scribe", "checker-2"] {
            assert!(is_safe_agent_name(name));
            assert!(!name.contains('/'));
            assert!(!name.contains('\\'));
            assert!(!name.starts_with('.'));
            assert_eq!(std::path::Path::new(name).components().count(), 1);
        }
    }
}
