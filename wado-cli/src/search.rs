//! The query and ranking `wado spec --search` and `wado doc --search` share.

/// How many hits a search prints before it counts the rest.
pub const SHOWN_HITS: usize = 20;

/// A query: whitespace-separated terms, matched without regard to case.
pub struct Query {
    terms: Vec<String>,
}

impl Query {
    /// `None` when `text` holds no term.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let terms: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
        (!terms.is_empty()).then_some(Self { terms })
    }

    /// The score of an entry whose text is `fields`, each `(text, weight)`, or
    /// `None` when a term is in none of them. A term counts the heaviest field
    /// holding it.
    #[must_use]
    pub fn score(&self, fields: &[(&str, u32)]) -> Option<u32> {
        let lowered: Vec<(String, u32)> = fields
            .iter()
            .map(|(text, weight)| (text.to_lowercase(), *weight))
            .collect();
        self.terms.iter().try_fold(0, |total, term| {
            let best = lowered
                .iter()
                .filter(|(text, _)| text.contains(term.as_str()))
                .map(|(_, weight)| *weight)
                .max()?;
            Some(total + best)
        })
    }

    /// Whether the whole query is `name`, as a reader typing a name expects.
    #[must_use]
    pub fn is_exactly(&self, name: &str) -> bool {
        self.terms.len() == 1 && self.terms[0] == name.to_lowercase()
    }

    /// The line of `text` holding the most terms, trimmed and cut to fit a
    /// terminal line; `None` when no line holds one.
    #[must_use]
    pub fn snippet(&self, text: &str) -> Option<String> {
        const WIDTH: usize = 100;
        let (count, line) = text
            .lines()
            .map(|line| {
                let lowered = line.to_lowercase();
                let count = self
                    .terms
                    .iter()
                    .filter(|t| lowered.contains(t.as_str()))
                    .count();
                (count, line.trim())
            })
            // `max_by_key` keeps the last of equals; the first reads better.
            .rev()
            .max_by_key(|(count, _)| *count)?;
        if count == 0 {
            return None;
        }
        Some(match line.char_indices().nth(WIDTH) {
            Some((cut, _)) => format!("{}…", &line[..cut]),
            None => line.to_string(),
        })
    }
}

/// `hits` best first, document order kept among equal scores.
pub fn rank<T>(mut hits: Vec<(u32, T)>) -> Vec<T> {
    hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    hits.into_iter().map(|(_, hit)| hit).collect()
}

/// The line under the shown hits that counts the rest, if any were cut.
#[must_use]
pub fn rest_line(total: usize) -> Option<String> {
    (total > SHOWN_HITS).then(|| {
        format!(
            "… and {} more; add a term to narrow the search",
            total - SHOWN_HITS
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_term_must_match_somewhere() {
        let q = Query::parse("Let ELSE").unwrap();
        assert_eq!(q.score(&[("let", 10), ("an else branch", 1)]), Some(11));
        assert_eq!(q.score(&[("let", 10), ("nothing", 1)]), None);
    }

    #[test]
    fn a_term_counts_its_heaviest_field() {
        let q = Query::parse("map").unwrap();
        assert_eq!(q.score(&[("map", 10), ("a map", 1)]), Some(10));
    }

    #[test]
    fn a_blank_query_is_none() {
        assert!(Query::parse("  ").is_none());
    }

    #[test]
    fn rank_is_stable_among_equals() {
        assert_eq!(
            rank(vec![(1, 'a'), (3, 'b'), (1, 'c')]),
            vec!['b', 'a', 'c']
        );
    }

    #[test]
    fn snippet_takes_the_first_line_with_the_most_terms() {
        let q = Query::parse("let else").unwrap();
        let text = "intro\n  let x = 1;\n  let Some(x) = o else { return; };\nlet else";
        assert_eq!(
            q.snippet(text).as_deref(),
            Some("let Some(x) = o else { return; };")
        );
        assert_eq!(q.snippet("nothing here"), None);
    }

    #[test]
    fn snippet_cuts_on_a_character_boundary() {
        let q = Query::parse("あ").unwrap();
        let long = "あ".repeat(150);
        let cut = q.snippet(&long).unwrap();
        assert_eq!(cut.chars().count(), 101);
        assert!(cut.ends_with('…'));
    }
}
