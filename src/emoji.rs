//! 絵文字一覧の控えと、前回との差分。
//!
//! [`EmojiSet`] は名前と URL で引ける絵文字の集まりで、状態ファイル `emojis.json` の中身でもある。
//! [`diff`] は旧 C# 版の `CheckDifference` の移植で、純粋関数にしてテストする。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::misskey::Emoji;

/// 状態ファイルの名前。
pub fn state_file(state_dir: &Path) -> PathBuf {
    state_dir.join("emojis.json")
}

/// 控えている絵文字 1 つ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEmoji {
    pub name: String,
    pub category: Option<String>,
    pub aliases: BTreeSet<String>,
    /// `emojis` が返す URL。
    pub url: String,
    /// 取った画像。取っていなければ `None`。
    pub image: Option<ImageInfo>,
}

/// 取った画像の控え。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// リダイレクトを追った後の URL。リンク先に使う。
    pub actual_url: String,
    /// 画像の SHA-1（16 進）。画像の変更の判定と、画像ファイルの名前に使う。
    pub sha1: String,
    /// 画像ファイルの拡張子（png、gif など）。
    pub format: String,
    /// 応答の `ETag`。次の取得の条件付き要求に使う。
    pub etag: Option<String>,
}

impl ImageInfo {
    /// 画像ファイルの名前。
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.sha1, self.format)
    }
}

impl StoredEmoji {
    /// 画像ファイルの名前。取っていなければ `None`。
    pub fn image_file(&self) -> Option<String> {
        self.image.as_ref().map(ImageInfo::file_name)
    }

    /// 画像リンクの先。リダイレクト後の URL があればそれ。
    pub fn link(&self) -> &str {
        self.image
            .as_ref()
            .map_or(self.url.as_str(), |image| image.actual_url.as_str())
    }
}

/// 名前で引ける絵文字の集まり。URL でも引ける（名前変更の検知に使う）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmojiSet {
    by_name: BTreeMap<String, StoredEmoji>,
    /// URL ごとの、その URL を使う絵文字の名前。同じドライブのファイルを使う絵文字は同じ URL になる。
    by_url: HashMap<String, BTreeSet<String>>,
}

impl EmojiSet {
    /// 控えから作る。同名の重複は最初の 1 件だけ採る。同じ URL（同じドライブのファイル）の絵文字はどれも残す。
    pub fn from_stored(emojis: impl IntoIterator<Item = StoredEmoji>) -> Self {
        let mut set = Self::default();
        for emoji in emojis {
            if set.by_name.contains_key(&emoji.name) {
                continue;
            }
            set.by_url
                .entry(emoji.url.clone())
                .or_default()
                .insert(emoji.name.clone());
            set.by_name.insert(emoji.name.clone(), emoji);
        }
        set
    }

    /// API の応答から作る。画像はまだ無い。
    pub fn from_api(emojis: impl IntoIterator<Item = Emoji>) -> Self {
        Self::from_stored(emojis.into_iter().map(|emoji| StoredEmoji {
            name: emoji.name,
            category: emoji.category,
            aliases: emoji.aliases.into_iter().collect(),
            url: emoji.url,
            image: None,
        }))
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&StoredEmoji> {
        self.by_name.get(name)
    }

    /// `url` を使う絵文字のうち、名前順で最初のもの。
    pub fn get_by_url(&self, url: &str) -> Option<&StoredEmoji> {
        self.names_by_url(url)
            .next()
            .and_then(|name| self.get(name))
    }

    /// `url` を使う絵文字の名前。名前順。
    fn names_by_url(&self, url: &str) -> impl Iterator<Item = &str> {
        self.by_url
            .get(url)
            .into_iter()
            .flat_map(|names| names.iter().map(String::as_str))
    }

    /// 名前順。状態ファイルにはこの順に書く。
    pub fn iter(&self) -> impl Iterator<Item = &StoredEmoji> {
        self.by_name.values()
    }

    /// `name` の画像を差し替える。
    pub fn set_image(&mut self, name: &str, image: Option<ImageInfo>) {
        if let Some(emoji) = self.by_name.get_mut(name) {
            emoji.image = image;
        }
    }

    /// `categories` のどれかに入る絵文字。`categories` が空なら全部。
    pub fn in_categories<'a>(&'a self, categories: &'a [String]) -> Vec<&'a StoredEmoji> {
        self.iter()
            .filter(|emoji| {
                categories.is_empty()
                    || emoji
                        .category
                        .as_ref()
                        .is_some_and(|category| categories.contains(category))
            })
            .collect()
    }
}

/// 前回との差分。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes<'a> {
    pub added: Vec<&'a StoredEmoji>,
    pub deleted: Vec<&'a StoredEmoji>,
    /// 名前が変わった（旧、新）。同じ URL で名前だけ違うもの。
    pub renamed: Vec<(&'a StoredEmoji, &'a StoredEmoji)>,
    pub image_changed: Vec<(&'a StoredEmoji, &'a StoredEmoji)>,
    pub category_changed: Vec<(&'a StoredEmoji, &'a StoredEmoji)>,
    pub alias_changed: Vec<(&'a StoredEmoji, &'a StoredEmoji)>,
}

impl Changes<'_> {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.deleted.is_empty()
            && self.renamed.is_empty()
            && self.image_changed.is_empty()
            && self.category_changed.is_empty()
            && self.alias_changed.is_empty()
    }
}

/// `old` から `new` への差分。
///
/// 同名が無ければ、同じ URL の旧項目のうち、新側に同名が無く、まだ名前変更の元にしていない最初のものからの名前変更、
/// それも無ければ追加。同名か名前変更の元の旧項目とは、画像（SHA-1 が両方あって違う）・カテゴリ・タグの差を取る。
/// 新側に対応が無い旧項目は削除。
pub fn diff<'a>(old: &'a EmojiSet, new: &'a EmojiSet) -> Changes<'a> {
    let mut changes = Changes::default();
    let mut kept: BTreeSet<&str> = BTreeSet::new();
    for emoji in new.iter() {
        let previous = match old.get(&emoji.name) {
            Some(previous) => previous,
            // 同じ URL の旧項目が新側に名前のまま残っていれば、名前変更ではなく追加。
            // 同じファイルを使う絵文字が複数あるので、1 つの旧項目を 2 つの名前変更の元にはしない。
            None => match old
                .names_by_url(&emoji.url)
                .find(|name| new.get(name).is_none() && !kept.contains(name))
                .and_then(|name| old.get(name))
            {
                Some(previous) => {
                    changes.renamed.push((previous, emoji));
                    previous
                }
                None => {
                    changes.added.push(emoji);
                    continue;
                }
            },
        };
        // 管理画面では名前とカテゴリなどを 1 回の保存で変えられるので、名前変更の組でも差を取る。
        if let (Some(a), Some(b)) = (&previous.image, &emoji.image)
            && a.sha1 != b.sha1
        {
            changes.image_changed.push((previous, emoji));
        }
        if previous.category != emoji.category {
            changes.category_changed.push((previous, emoji));
        }
        if previous.aliases != emoji.aliases {
            changes.alias_changed.push((previous, emoji));
        }
        kept.insert(&previous.name);
    }
    changes.deleted = old
        .iter()
        .filter(|emoji| !kept.contains(emoji.name.as_str()))
        .collect();
    changes
}

#[cfg(test)]
pub mod test_support {
    use super::*;

    pub fn stored(name: &str, category: Option<&str>, aliases: &[&str], url: &str) -> StoredEmoji {
        StoredEmoji {
            name: name.to_owned(),
            category: category.map(str::to_owned),
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
            url: url.to_owned(),
            image: None,
        }
    }

    pub fn with_image(mut emoji: StoredEmoji, sha1: &str) -> StoredEmoji {
        emoji.image = Some(ImageInfo {
            actual_url: format!("{}?v={sha1}", emoji.url),
            sha1: sha1.to_owned(),
            format: "png".to_owned(),
            etag: None,
        });
        emoji
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{stored, with_image};
    use super::*;

    fn names<'a>(emojis: impl IntoIterator<Item = &'a StoredEmoji>) -> Vec<&'a str> {
        emojis.into_iter().map(|e| e.name.as_str()).collect()
    }

    fn pair_names<'a>(pairs: &[(&'a StoredEmoji, &'a StoredEmoji)]) -> Vec<(&'a str, &'a str)> {
        pairs
            .iter()
            .map(|(a, b)| (a.name.as_str(), b.name.as_str()))
            .collect()
    }

    #[test]
    fn duplicate_names_keep_the_first_and_shared_urls_keep_every_emoji() {
        let set = EmojiSet::from_stored([
            stored("a", None, &[], "u1"),
            stored("a", None, &[], "u2"),
            stored("b", None, &[], "u1"),
            stored("c", None, &[], "u3"),
        ]);
        assert_eq!(names(set.iter()), ["a", "b", "c"]);
        assert_eq!(set.get_by_url("u1").unwrap().name, "a");
    }

    #[test]
    fn a_second_emoji_on_the_same_file_is_an_addition_not_a_rename() {
        let old = EmojiSet::from_stored([stored("a", None, &[], "u")]);
        let new = EmojiSet::from_stored([stored("a", None, &[], "u"), stored("b", None, &[], "u")]);
        let changes = diff(&old, &new);
        assert_eq!(names(changes.added.iter().copied()), ["b"]);
        assert!(changes.renamed.is_empty());
    }

    #[test]
    fn a_rename_and_a_new_emoji_on_the_same_file_are_a_rename_and_an_addition() {
        let old = EmojiSet::from_stored([stored("bar", None, &[], "u")]);
        let new = EmojiSet::from_stored([
            stored("copy", None, &[], "u"),
            stored("renamed", None, &[], "u"),
        ]);
        let changes = diff(&old, &new);
        assert_eq!(pair_names(&changes.renamed), [("bar", "copy")]);
        assert_eq!(names(changes.added.iter().copied()), ["renamed"]);
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn a_rename_also_reports_the_changes_made_with_it() {
        let old = EmojiSet::from_stored([with_image(stored("a", Some("x"), &["t"], "u"), "h1")]);
        let new = EmojiSet::from_stored([with_image(stored("b", Some("y"), &["s"], "u"), "h2")]);
        let changes = diff(&old, &new);
        assert_eq!(pair_names(&changes.renamed), [("a", "b")]);
        assert_eq!(pair_names(&changes.image_changed), [("a", "b")]);
        assert_eq!(pair_names(&changes.category_changed), [("a", "b")]);
        assert_eq!(pair_names(&changes.alias_changed), [("a", "b")]);
    }

    #[test]
    fn renaming_either_emoji_on_a_shared_file_is_a_rename() {
        let old = EmojiSet::from_stored([stored("a", None, &[], "u"), stored("b", None, &[], "u")]);
        for (kept, renamed, to) in [("a", "b", "c"), ("b", "a", "c")] {
            let new =
                EmojiSet::from_stored([stored(kept, None, &[], "u"), stored(to, None, &[], "u")]);
            let changes = diff(&old, &new);
            assert_eq!(pair_names(&changes.renamed), [(renamed, to)], "{kept}");
            assert!(changes.added.is_empty(), "{changes:?}");
            assert!(changes.deleted.is_empty(), "{changes:?}");
        }
    }

    #[test]
    fn renaming_every_emoji_on_a_shared_file_pairs_them_up() {
        let old = EmojiSet::from_stored([stored("a", None, &[], "u"), stored("b", None, &[], "u")]);
        let new = EmojiSet::from_stored([stored("c", None, &[], "u"), stored("d", None, &[], "u")]);
        let changes = diff(&old, &new);
        assert_eq!(pair_names(&changes.renamed), [("a", "c"), ("b", "d")]);
        assert!(changes.added.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn diff_finds_every_kind_of_change() {
        let old = EmojiSet::from_stored([
            with_image(stored("same", Some("c1"), &["x"], "u-same"), "h1"),
            with_image(stored("gone", None, &[], "u-gone"), "h2"),
            with_image(stored("old_name", None, &[], "u-renamed"), "h3"),
            with_image(stored("pic", None, &[], "u-pic"), "h4"),
            with_image(stored("cat", Some("c1"), &[], "u-cat"), "h5"),
            with_image(stored("tag", None, &["a", ""], "u-tag"), "h6"),
            stored("no_image", None, &[], "u-noimg"),
        ]);
        let new = EmojiSet::from_stored([
            with_image(stored("same", Some("c1"), &["x"], "u-same"), "h1"),
            with_image(stored("new_name", None, &[], "u-renamed"), "h3"),
            with_image(stored("pic", None, &[], "u-pic2"), "h4-changed"),
            with_image(stored("cat", Some("c2"), &[], "u-cat"), "h5"),
            with_image(stored("tag", None, &["b"], "u-tag"), "h6"),
            with_image(stored("no_image", None, &[], "u-noimg"), "h7"),
            with_image(stored("added", None, &[], "u-added"), "h8"),
        ]);
        let changes = diff(&old, &new);
        assert_eq!(names(changes.added.iter().copied()), ["added"]);
        assert_eq!(names(changes.deleted.iter().copied()), ["gone"]);
        assert_eq!(pair_names(&changes.renamed), [("old_name", "new_name")]);
        assert_eq!(pair_names(&changes.image_changed), [("pic", "pic")]);
        assert_eq!(pair_names(&changes.category_changed), [("cat", "cat")]);
        assert_eq!(pair_names(&changes.alias_changed), [("tag", "tag")]);
    }

    #[test]
    fn a_missing_image_on_either_side_is_not_an_image_change() {
        let old = EmojiSet::from_stored([stored("a", None, &[], "u")]);
        let new = EmojiSet::from_stored([with_image(stored("a", None, &[], "u"), "h")]);
        let changes = diff(&old, &new);
        assert!(changes.is_empty(), "{changes:?}");
        assert!(diff(&new, &old).is_empty());
    }

    #[test]
    fn a_renamed_emoji_is_not_also_deleted() {
        let old = EmojiSet::from_stored([stored("a", None, &[], "u")]);
        let new = EmojiSet::from_stored([stored("b", None, &[], "u")]);
        let changes = diff(&old, &new);
        assert_eq!(pair_names(&changes.renamed), [("a", "b")]);
        assert!(changes.deleted.is_empty());
        assert!(changes.added.is_empty());
    }

    #[test]
    fn the_first_snapshot_against_an_empty_set_adds_everything() {
        let new = EmojiSet::from_stored([stored("a", None, &[], "u")]);
        let empty = EmojiSet::default();
        let changes = diff(&empty, &new);
        assert_eq!(names(changes.added.iter().copied()), ["a"]);
    }

    #[test]
    fn categories_filter_and_empty_means_all() {
        let set = EmojiSet::from_stored([
            stored("a", Some("c1"), &[], "u1"),
            stored("b", Some("c2"), &[], "u2"),
            stored("c", None, &[], "u3"),
        ]);
        assert_eq!(names(set.in_categories(&[])), ["a", "b", "c"]);
        assert_eq!(names(set.in_categories(&["c2".to_owned()])), ["b"]);
        assert!(set.in_categories(&["none".to_owned()]).is_empty());
    }

    #[test]
    fn link_prefers_the_actual_url() {
        let plain = stored("a", None, &[], "u");
        assert_eq!(plain.link(), "u");
        assert_eq!(plain.image_file(), None);
        let imaged = with_image(plain, "h");
        assert_eq!(imaged.link(), "u?v=h");
        assert_eq!(imaged.image_file().as_deref(), Some("h.png"));
    }
}
