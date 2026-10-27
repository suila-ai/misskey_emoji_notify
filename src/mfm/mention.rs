//! ガチャと占いの返信の本文。

/// ガチャの結果。引いた絵文字を `:name:` で並べる。
pub fn gacha(names: &[&str]) -> String {
    names.iter().map(|name| format!(":{name}:")).collect()
}

/// 吉か凶か。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Luck {
    Good,
    Bad,
}

/// 占いの結果。
pub fn fortune(emoji: &str, luck: Luck, lucky_emoji: &str) -> String {
    let suffix = match luck {
        Luck::Good => "吉",
        Luck::Bad => "凶",
    };
    format!("今日のあなたの運勢は\n:{emoji}:{suffix}\n幸運の絵文字: :{lucky_emoji}:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gacha_concatenates_the_names() {
        assert_eq!(gacha(&["a", "b", "a"]), ":a::b::a:");
        assert_eq!(gacha(&[]), "");
    }

    #[test]
    fn fortune_matches_the_old_layout() {
        assert_eq!(
            fortune("cat", Luck::Good, "dog"),
            "今日のあなたの運勢は\n:cat:吉\n幸運の絵文字: :dog:"
        );
        assert_eq!(
            fortune("cat", Luck::Bad, "dog"),
            "今日のあなたの運勢は\n:cat:凶\n幸運の絵文字: :dog:"
        );
    }
}
