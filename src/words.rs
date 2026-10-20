//! Small helpers for the sentences the product writes in plain text.
//!
//! The product's own voice does not carry a pluralisation gutter: a record that reads
//! like an English sentence says `1 turn` and `2 turns`, never `turn(s)`. The JSONL
//! transcript keeps its numbers, and this is only for the plain-text lines a person
//! reads on a screen or in the log.

/// A count with the noun in the right number: `1 turn`, `2 turns`.
///
/// Both forms are passed in because English is not a rule here (`repository` /
/// `repositories`). The count is written in front, so the sentence reads naturally
/// wherever it is placed.
pub fn counted(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

/// A file size in the shortest unit that keeps it readable: `812 B`, `4.0 KiB`.
pub fn filesize_text(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        return format!("{kib:.1} KiB");
    }
    let mib = kib / 1024.0;
    if mib < 1024.0 {
        return format!("{mib:.1} MiB");
    }
    format!("{:.1} GiB", mib / 1024.0)
}

/// A login and its email, joined so a screen never splits them at their own space.
///
/// `<login> <email>` is one fact, and word wrap consumes the separating space at a
/// line edge: the next row began with the email, so `someone-else` and
/// `<someone@else.invalid>` read as one glued token (N-2). A no-break space renders
/// as a space and can never be the break point, so the pair wraps whole and the two
/// tokens stay visibly apart. The value the product acts on is unchanged: `git` is
/// configured with its own ASCII space, and this is only what a screen draws.
pub fn identity_pair(login: &str, email: &str) -> String {
    format!("{login}\u{a0}<{email}>")
}

/// Join an identity that is already one string (`name <email>`) without a break at
/// its own space: the first ` <` becomes a no-break space (N-2).
pub fn join_identity(identity: &str) -> String {
    identity.replacen(" <", "\u{a0}<", 1)
}

#[cfg(test)]
mod tests {
    use super::{counted, filesize_text, identity_pair, join_identity};

    #[test]
    fn a_count_reads_as_a_sentence() {
        assert_eq!(counted(1, "turn", "turns"), "1 turn");
        assert_eq!(counted(2, "turn", "turns"), "2 turns");
        assert_eq!(counted(0, "turn", "turns"), "0 turns");
        assert_eq!(counted(1, "repository", "repositories"), "1 repository");
        assert_eq!(counted(3, "repository", "repositories"), "3 repositories");
    }

    #[test]
    fn a_file_size_is_readable_in_the_right_unit() {
        assert_eq!(filesize_text(812), "812 B");
        assert_eq!(filesize_text(4096), "4.0 KiB");
        assert_eq!(filesize_text(2 * 1024 * 1024), "2.0 MiB");
    }

    #[test]
    fn an_identity_pair_keeps_a_space_that_a_wrap_cannot_eat() {
        // N-2: the pair reads `login <email>`; the separator is a no-break space so a
        // wrap can never land on it and leave the email glued to the login.
        let pair = identity_pair("someone-else", "someone@else.invalid");
        assert_eq!(pair, "someone-else\u{a0}<someone@else.invalid>");
        assert!(!pair.contains(" \u{a0}"), "the pair has no breakable space");
        assert_eq!(join_identity("someone-else <someone@else.invalid>"), pair);
        // A bare name is left as it is: there is no pair to protect.
        assert_eq!(join_identity("someone-else"), "someone-else");
    }
}
