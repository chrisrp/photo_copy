//! Multi-selection of files in the current folder, Finder style. UI independent.
//! Files are identified by name, so the selection survives re-listing the folder.

use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct Selection {
    names: HashSet<String>,
    /// Where a Shift-click range starts: the last file clicked without Shift.
    anchor: Option<String>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn clear(&mut self) {
        self.names.clear();
        self.anchor = None;
    }

    pub fn select_all(&mut self, order: &[String]) {
        self.names = order.iter().cloned().collect();
        self.anchor = None;
    }

    /// Apply a click on `clicked`. `order` is every selectable file in display order.
    /// - plain click: select only `clicked`
    /// - `toggle` (Cmd/Ctrl): add or remove `clicked`
    /// - `range` (Shift): select from the anchor to `clicked` (added to the current
    ///   selection when `toggle` is also held)
    pub fn click(&mut self, order: &[String], clicked: &str, toggle: bool, range: bool) {
        let position = |name: &str| order.iter().position(|n| n == name);
        let anchor = if range {
            self.anchor.as_deref().and_then(position)
        } else {
            None
        };
        match (anchor, position(clicked)) {
            (Some(from), Some(to)) => {
                if !toggle {
                    self.names.clear();
                }
                self.names
                    .extend(order[from.min(to)..=from.max(to)].iter().cloned());
            }
            _ if toggle => {
                if !self.names.remove(clicked) {
                    self.names.insert(clicked.to_string());
                }
                self.anchor = Some(clicked.to_string());
            }
            _ => {
                self.names.clear();
                self.names.insert(clicked.to_string());
                self.anchor = Some(clicked.to_string());
            }
        }
    }

    /// Forget selected files that are no longer in `existing`.
    pub fn retain_existing(&mut self, existing: &HashSet<&str>) {
        self.names.retain(|n| existing.contains(n.as_str()));
        if self
            .anchor
            .as_deref()
            .is_some_and(|a| !existing.contains(a))
        {
            self.anchor = None;
        }
    }

    /// The selected names in display order.
    pub fn in_order(&self, order: &[String]) -> Vec<String> {
        order
            .iter()
            .filter(|n| self.names.contains(n.as_str()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> Vec<String> {
        ["a", "b", "c", "d", "e"].map(String::from).to_vec()
    }

    fn names(selection: &Selection) -> Vec<String> {
        selection.in_order(&order())
    }

    #[test]
    fn plain_click_selects_only_that_file() {
        let mut s = Selection::default();

        s.click(&order(), "b", false, false);
        assert_eq!(names(&s), ["b"]);

        s.click(&order(), "d", false, false);
        assert_eq!(names(&s), ["d"]);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn toggle_click_adds_and_removes() {
        let mut s = Selection::default();
        s.click(&order(), "a", false, false);

        s.click(&order(), "c", true, false);
        assert_eq!(names(&s), ["a", "c"]);

        s.click(&order(), "a", true, false);
        assert_eq!(names(&s), ["c"]);
        assert!(s.contains("c") && !s.contains("a"));
    }

    #[test]
    fn shift_click_selects_a_range_from_the_anchor_in_either_direction() {
        let mut s = Selection::default();
        s.click(&order(), "b", false, false);

        s.click(&order(), "d", false, true);
        assert_eq!(names(&s), ["b", "c", "d"]);

        s.click(&order(), "a", false, true);
        assert_eq!(names(&s), ["a", "b"]);
    }

    #[test]
    fn shift_with_toggle_extends_the_selection() {
        let mut s = Selection::default();
        s.click(&order(), "a", false, false);
        s.click(&order(), "c", true, false);

        s.click(&order(), "e", true, true);

        assert_eq!(names(&s), ["a", "c", "d", "e"]);
    }

    #[test]
    fn shift_click_without_anchor_acts_like_a_plain_click() {
        let mut s = Selection::default();

        s.click(&order(), "c", false, true);

        assert_eq!(names(&s), ["c"]);
    }

    #[test]
    fn select_all_and_clear() {
        let mut s = Selection::default();

        s.select_all(&order());
        assert_eq!(s.len(), 5);

        s.clear();
        assert!(s.is_empty());
    }

    #[test]
    fn in_order_follows_the_display_order_not_click_order() {
        let mut s = Selection::default();
        s.click(&order(), "d", false, false);
        s.click(&order(), "a", true, false);
        s.click(&order(), "c", true, false);

        assert_eq!(names(&s), ["a", "c", "d"]);
        let reversed: Vec<String> = order().into_iter().rev().collect();
        assert_eq!(s.in_order(&reversed), ["d", "c", "a"]);
    }

    #[test]
    fn retain_existing_drops_vanished_files_and_stale_anchor() {
        let mut s = Selection::default();
        s.click(&order(), "a", false, false);
        s.click(&order(), "b", true, false);

        s.retain_existing(&HashSet::from(["a", "c"]));

        assert_eq!(names(&s), ["a"]);
        // The anchor "b" is gone, so a Shift-click starts fresh.
        s.click(&order(), "d", false, true);
        assert_eq!(names(&s), ["d"]);
    }
}
