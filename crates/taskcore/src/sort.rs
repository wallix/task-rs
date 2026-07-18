//! Task-listing sort strategies. Each sorter reorders `items` in place;
//! `namespaces` carries the set of namespace prefixes in effect, which
//! influences the root-tasks-first variant.

/// A sort strategy applied to a task list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sorter {
    /// Leave items in their definition order.
    None,
    /// Sort alphanumerically by task name.
    AlphaNumeric,
    /// Sort alphanumerically, but list non-namespaced tasks before
    /// namespaced ones (detected by the presence of a `:`).
    AlphaNumericWithRootTasksFirst,
}

impl Sorter {
    /// Applies the strategy, reordering `items` in place.
    pub fn sort(self, items: &mut [String], namespaces: &[String]) {
        match self {
            Sorter::None => no_sort(items, namespaces),
            Sorter::AlphaNumeric => alpha_numeric(items, namespaces),
            Sorter::AlphaNumericWithRootTasksFirst => {
                alpha_numeric_with_root_tasks_first(items, namespaces)
            }
        }
    }
}

/// Leaves the tasks in the order they are defined.
pub fn no_sort(_items: &mut [String], _namespaces: &[String]) {}

/// Sorts tasks into alphanumeric order by name.
pub fn alpha_numeric(items: &mut [String], _namespaces: &[String]) {
    items.sort();
}

/// Sorts tasks alphanumerically, but ensures non-namespaced tasks are
/// listed before namespaced ones. When any namespaces are present the plain
/// alphanumeric order is used instead.
pub fn alpha_numeric_with_root_tasks_first(items: &mut [String], namespaces: &[String]) {
    if !namespaces.is_empty() {
        alpha_numeric(items, namespaces);
        return;
    }
    items.sort_by(|a, b| {
        let a_ns = a.contains(':');
        let b_ns = b.contains(':');
        match (a_ns, b_ns) {
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn root_tasks_first_no_namespace_alpha() {
        let mut items = v(&["ns1:item3", "m-item2", "a-item1"]);
        alpha_numeric_with_root_tasks_first(&mut items, &[]);
        assert_eq!(items, v(&["a-item1", "m-item2", "ns1:item3"]));
    }

    #[test]
    fn root_tasks_first_namespaced_after() {
        let mut items = v(&["ns1:item3", "ns2:item4", "z-item5"]);
        alpha_numeric_with_root_tasks_first(&mut items, &[]);
        assert_eq!(items, v(&["z-item5", "ns1:item3", "ns2:item4"]));
    }

    #[test]
    fn root_tasks_first_full() {
        let mut items = v(&[
            "ns3:item6",
            "z-item5",
            "ns2:item4",
            "ns1:item3",
            "m-item2",
            "a-item1",
        ]);
        alpha_numeric_with_root_tasks_first(&mut items, &[]);
        assert_eq!(
            items,
            v(&[
                "a-item1",
                "m-item2",
                "z-item5",
                "ns1:item3",
                "ns2:item4",
                "ns3:item6"
            ])
        );
    }

    #[test]
    fn alpha_numeric_sort() {
        let mut items = v(&[
            "ns1:item3",
            "m-item2",
            "z-item5",
            "a-item1",
            "ns2:item4",
            "ns3:item6",
        ]);
        alpha_numeric(&mut items, &[]);
        assert_eq!(
            items,
            v(&[
                "a-item1",
                "m-item2",
                "ns1:item3",
                "ns2:item4",
                "ns3:item6",
                "z-item5"
            ])
        );
    }

    #[test]
    fn no_sort_keeps_order() {
        let original = v(&[
            "ns1:item3",
            "m-item2",
            "z-item5",
            "a-item1",
            "ns2:item4",
            "ns3:item6",
        ]);
        let mut items = original.clone();
        no_sort(&mut items, &[]);
        assert_eq!(items, original);
    }

    #[test]
    fn sorter_enum_dispatch() {
        let mut items = v(&["b", "a", "c"]);
        Sorter::AlphaNumeric.sort(&mut items, &[]);
        assert_eq!(items, v(&["a", "b", "c"]));
    }
}
