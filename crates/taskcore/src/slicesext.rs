//! Small generic slice helpers.

/// Concatenates all input slices, sorts the result, and removes
/// consecutive duplicates, yielding a sorted set-like vector.
pub fn unique_join<T: Ord + Clone>(slices: &[&[T]]) -> Vec<T> {
    let mut out: Vec<T> = slices.iter().flat_map(|s| s.iter().cloned()).collect();
    out.sort();
    out.dedup();
    out
}

/// Maps each element of `s` through `f`, preserving order and length.
pub fn convert<T, U, F: FnMut(&T) -> U>(s: &[T], mut f: F) -> Vec<U> {
    s.iter().map(&mut f).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_int_to_string() {
        let input = [1, 2, 3, 4, 5];
        let result = convert(&input, |n| n.to_string());
        assert_eq!(result, vec!["1", "2", "3", "4", "5"]);
    }

    #[test]
    fn convert_string_to_int() {
        let input = ["1", "2", "3", "4", "5"];
        let result = convert(&input, |s| s.parse::<i64>().unwrap_or(0));
        assert_eq!(result, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn convert_float_to_int() {
        let input = [1.1_f64, 2.2, 3.7, 4.5, 5.9];
        let result = convert(&input, |f| f.round() as i64);
        assert_eq!(result, vec![1, 2, 4, 5, 6]);
    }

    #[test]
    fn convert_empty() {
        let input: [i64; 0] = [];
        let result = convert(&input, |n| n.to_string());
        assert!(result.is_empty());
    }

    #[test]
    fn unique_join_sorts_and_dedups() {
        let a = [3, 1, 2];
        let b = [2, 4, 1];
        let result = unique_join(&[&a[..], &b[..]]);
        assert_eq!(result, vec![1, 2, 3, 4]);
    }

    #[test]
    fn unique_join_empty() {
        let result: Vec<i64> = unique_join(&[]);
        assert!(result.is_empty());
    }
}
