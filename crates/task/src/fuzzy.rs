//! A small "did you mean" suggester based on Levenshtein edit distance.
//!
//! The Go build uses `sajari/fuzzy`; here a hand-rolled edit-distance search
//! over the candidate task names is enough to point the user at the nearest
//! match when they mistype a task name.

/// Returns the candidate nearest to `target` within an edit-distance threshold,
/// or `None` when nothing is close enough. The threshold scales with the target
/// length so short names require a closer match.
pub fn suggest<'a, I>(target: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let threshold = threshold_for(target);
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let distance = levenshtein(target, candidate);
        if distance > threshold {
            continue;
        }
        match best {
            Some((best_distance, _)) if best_distance <= distance => {}
            _ => best = Some((distance, candidate)),
        }
    }
    best.map(|(_, name)| name)
}

/// The maximum edit distance accepted for a target of the given length.
fn threshold_for(target: &str) -> usize {
    match target.chars().count() {
        0..=3 => 1,
        4..=6 => 2,
        _ => 3,
    }
}

/// Computes the Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    // A single rolling row of the edit-distance matrix.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len().saturating_add(1)];

    for (i, ca) in a.iter().enumerate() {
        if let Some(slot) = curr.first_mut() {
            *slot = i.saturating_add(1);
        }
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            let deletion = prev.get(j.saturating_add(1)).copied().unwrap_or(usize::MAX);
            let insertion = curr.get(j).copied().unwrap_or(usize::MAX);
            let substitution = prev.get(j).copied().unwrap_or(usize::MAX);
            let best = deletion
                .saturating_add(1)
                .min(insertion.saturating_add(1))
                .min(substitution.saturating_add(cost));
            if let Some(slot) = curr.get_mut(j.saturating_add(1)) {
                *slot = best;
            }
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev.last().copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_basics() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("build", "build"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn suggests_nearest() {
        let names = ["build", "test", "deploy"];
        assert_eq!(suggest("buld", names), Some("build"));
        assert_eq!(suggest("tset", names), Some("test"));
    }

    #[test]
    fn no_suggestion_when_far() {
        let names = ["build", "test"];
        assert_eq!(suggest("xyzzyplugh", names), None);
    }
}
