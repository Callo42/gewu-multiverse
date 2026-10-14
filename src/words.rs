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

#[cfg(test)]
mod tests {
    use super::counted;

    #[test]
    fn a_count_reads_as_a_sentence() {
        assert_eq!(counted(1, "turn", "turns"), "1 turn");
        assert_eq!(counted(2, "turn", "turns"), "2 turns");
        assert_eq!(counted(0, "turn", "turns"), "0 turns");
        assert_eq!(counted(1, "repository", "repositories"), "1 repository");
        assert_eq!(counted(3, "repository", "repositories"), "3 repositories");
    }
}
