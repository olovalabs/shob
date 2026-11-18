const BULLETS: [&str; 5] = ["•", "◦", "▪", "‣", "⁃"];

pub(super) fn list_item_prefix(ix: usize, ordered: bool, depth: usize) -> String {
    if ordered {
        format!("{}. ", ix + 1)
    } else {
        let depth = depth.min(BULLETS.len() - 1);
        format!("{} ", BULLETS[depth])
    }
}

#[cfg(test)]
mod tests {
    use crate::text::utils::list_item_prefix;

    #[test]
    fn test_list_item_prefix() {
        assert_eq!(list_item_prefix(0, true, 0), "1. ");
        assert_eq!(list_item_prefix(1, true, 0), "2. ");
        assert_eq!(list_item_prefix(2, true, 0), "3. ");
        assert_eq!(list_item_prefix(10, true, 0), "11. ");
        assert_eq!(list_item_prefix(0, true, 1), "1. ");
        assert_eq!(list_item_prefix(1, true, 1), "2. ");
        assert_eq!(list_item_prefix(2, true, 1), "3. ");
        assert_eq!(list_item_prefix(0, true, 2), "1. ");
        assert_eq!(list_item_prefix(1, true, 2), "2. ");
        assert_eq!(list_item_prefix(6, true, 2), "7. ");
        assert_eq!(list_item_prefix(0, false, 0), "• ");
        assert_eq!(list_item_prefix(0, false, 1), "◦ ");
        assert_eq!(list_item_prefix(0, false, 2), "▪ ");
        assert_eq!(list_item_prefix(0, false, 3), "‣ ");
        assert_eq!(list_item_prefix(0, false, 4), "⁃ ");
    }
}
