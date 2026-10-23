//! 絵文字の変更の通知文。旧版の `IntervalJobRunner` の各 `Post*` と同じ形。

use std::collections::BTreeMap;

use crate::emoji::{Changes, StoredEmoji};

use super::{MAX_NOTE_LENGTH, category, image_link, note_length};

/// 投稿する本文の列。種別ごとに 1 つ。カテゴリ変更は（旧、新）の組ごとに 1 つ。
/// ノートの上限を超える種別は、同じ見出しを付けて複数に分ける。
pub fn render(changes: &Changes<'_>) -> Vec<String> {
    render_limited(changes, MAX_NOTE_LENGTH)
}

/// [`render`] の上限を `limit` にしたもの。
fn render_limited(changes: &Changes<'_>, limit: usize) -> Vec<String> {
    let mut texts = Vec::new();
    texts.extend(added(&changes.added, limit));
    texts.extend(deleted(&changes.deleted, limit));
    texts.extend(renamed(&changes.renamed, limit));
    texts.extend(image_changed(&changes.image_changed, limit));
    texts.extend(category_changed(&changes.category_changed, limit));
    texts.extend(alias_changed(&changes.alias_changed, limit));
    texts
}

/// `header` のあとに `items` を `sep` でつないだ本文を、`limit` を超えないように分ける。分けた本文にも同じ見出しを付ける。
/// 1 項目だけで超えるときは、その項目だけで 1 つにする。
fn pack(header: &str, items: &[String], sep: &str, limit: usize) -> Vec<String> {
    let mut texts = Vec::new();
    let mut body = String::new();
    for item in items {
        if !body.is_empty() {
            let length = note_length(header) + note_length(&body) + note_length(sep);
            if length + note_length(item) > limit {
                texts.push(format!("{header}{body}"));
                body.clear();
            } else {
                body.push_str(sep);
            }
        }
        body.push_str(item);
    }
    if !body.is_empty() {
        texts.push(format!("{header}{body}"));
    }
    texts
}

/// 空でないタグを空白でつなぐ。無ければ `(なし)`。
fn aliases(emoji: &StoredEmoji) -> String {
    let joined = join_tags(emoji.aliases.iter().map(String::as_str));
    if joined.is_empty() {
        "(なし)".to_owned()
    } else {
        joined
    }
}

fn join_tags<'a>(tags: impl Iterator<Item = &'a str>) -> String {
    tags.filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn added(emojis: &[&StoredEmoji], limit: usize) -> Vec<String> {
    let items: Vec<String> = emojis
        .iter()
        .map(|e| {
            format!(
                "$[x2 :{name}:] `:{name}:`\nカテゴリ: <plain>{category}</plain>\nタグ: <plain>{aliases}</plain>\n{link}",
                name = e.name,
                category = category(e.category.as_deref()),
                aliases = aliases(e),
                link = image_link("画像", e.link()),
            )
        })
        .collect();
    pack("【絵文字追加】\n", &items, "\n\n", limit)
}

fn deleted(emojis: &[&StoredEmoji], limit: usize) -> Vec<String> {
    let items: Vec<String> = emojis
        .iter()
        .map(|e| format!("`:{}:`\n{}", e.name, image_link("旧画像", e.link())))
        .collect();
    pack("【絵文字削除】\n", &items, "\n\n", limit)
}

fn renamed(pairs: &[(&StoredEmoji, &StoredEmoji)], limit: usize) -> Vec<String> {
    let items: Vec<String> = pairs
        .iter()
        .map(|(old, new)| {
            format!(
                "$[x2 :{new}:]\n`:{old}:` → `:{new}:`",
                old = old.name,
                new = new.name
            )
        })
        .collect();
    pack("【名前変更】\n", &items, "\n\n", limit)
}

fn image_changed(pairs: &[(&StoredEmoji, &StoredEmoji)], limit: usize) -> Vec<String> {
    let items: Vec<String> = pairs
        .iter()
        .map(|(old, new)| {
            // リンク先が同じなら旧画像のリンクは出さない。
            let old_link = if old.link() != new.link() {
                format!("{} ", image_link("旧画像", old.link()))
            } else {
                String::new()
            };
            format!(
                "$[x2 :{name}:] `:{name}:`\n{old_link}{new_link}",
                name = new.name,
                new_link = image_link("新画像", new.link()),
            )
        })
        .collect();
    pack("【画像変更】\n", &items, "\n\n", limit)
}

fn category_changed(pairs: &[(&StoredEmoji, &StoredEmoji)], limit: usize) -> Vec<String> {
    // （旧、新）の組ごとに 1 投稿。組の順は旧カテゴリ、新カテゴリの名前順。
    let mut groups: BTreeMap<(Option<&str>, Option<&str>), Vec<&StoredEmoji>> = BTreeMap::new();
    for (old, new) in pairs {
        groups
            .entry((old.category.as_deref(), new.category.as_deref()))
            .or_default()
            .push(new);
    }
    groups
        .into_iter()
        .flat_map(|((old, new), emojis)| {
            let names: Vec<String> = emojis.iter().map(|e| format!(":{}:", e.name)).collect();
            let header = format!(
                "【カテゴリ変更】\n<plain>{} → {}</plain>\n",
                category(old),
                category(new),
            );
            pack(&header, &names, " ", limit)
        })
        .collect()
}

fn alias_changed(pairs: &[(&StoredEmoji, &StoredEmoji)], limit: usize) -> Vec<String> {
    let items: Vec<String> = pairs
        .iter()
        .filter_map(|(old, new)| {
            let add = join_tags(new.aliases.difference(&old.aliases).map(String::as_str));
            let delete = join_tags(old.aliases.difference(&new.aliases).map(String::as_str));
            if add.is_empty() && delete.is_empty() {
                return None;
            }
            let mut text = format!("$[x2 :{name}:] `:{name}:`\n", name = new.name);
            if !add.is_empty() {
                text.push_str(&format!("追加: <plain>{add}</plain>\n"));
            }
            if !delete.is_empty() {
                text.push_str(&format!("削除: <plain>{delete}</plain>\n"));
            }
            Some(text)
        })
        .collect();
    // 空文字のタグだけの差分なら、見出しだけの投稿はしない（旧版はしていた）。項目が無ければ pack は何も返さない。
    pack("【タグ変更】\n", &items, "\n", limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emoji::test_support::{stored, with_image};

    #[test]
    fn added_lists_category_tags_and_the_image_link() {
        let a = with_image(stored("a", Some("顔"), &["smile", ""], "https://s/a"), "h1");
        let b = stored("b", None, &[], "https://s/b");
        let changes = Changes {
            added: vec![&a, &b],
            ..Changes::default()
        };
        assert_eq!(
            render(&changes),
            ["【絵文字追加】\n\
              $[x2 :a:] `:a:`\nカテゴリ: <plain>顔</plain>\nタグ: <plain>smile</plain>\n?[画像](https://s/a?v=h1)\n\n\
              $[x2 :b:] `:b:`\nカテゴリ: <plain>(なし)</plain>\nタグ: <plain>(なし)</plain>\n?[画像](https://s/b)"]
        );
    }

    #[test]
    fn deleted_and_renamed() {
        let gone = with_image(stored("gone", None, &[], "https://s/g"), "h");
        let old = stored("old", None, &[], "https://s/r");
        let new = stored("new", None, &[], "https://s/r");
        let changes = Changes {
            deleted: vec![&gone],
            renamed: vec![(&old, &new)],
            ..Changes::default()
        };
        assert_eq!(
            render(&changes),
            [
                "【絵文字削除】\n`:gone:`\n?[旧画像](https://s/g?v=h)",
                "【名前変更】\n$[x2 :new:]\n`:old:` → `:new:`",
            ]
        );
    }

    #[test]
    fn image_changed_shows_the_old_link_only_when_it_differs() {
        let old1 = with_image(stored("p", None, &[], "https://s/p"), "h1");
        let new1 = with_image(stored("p", None, &[], "https://s/p"), "h2");
        let mut old2 = with_image(stored("q", None, &[], "https://s/q"), "h1");
        let mut new2 = with_image(stored("q", None, &[], "https://s/q"), "h2");
        old2.image.as_mut().unwrap().actual_url = "https://s/q".to_owned();
        new2.image.as_mut().unwrap().actual_url = "https://s/q".to_owned();
        let changes = Changes {
            image_changed: vec![(&old1, &new1), (&old2, &new2)],
            ..Changes::default()
        };
        assert_eq!(
            render(&changes),
            ["【画像変更】\n\
              $[x2 :p:] `:p:`\n?[旧画像](https://s/p?v=h1) ?[新画像](https://s/p?v=h2)\n\n\
              $[x2 :q:] `:q:`\n?[新画像](https://s/q)"]
        );
    }

    #[test]
    fn category_changes_are_grouped_by_the_pair() {
        let a_old = stored("a", Some("x"), &[], "u1");
        let a_new = stored("a", Some("y"), &[], "u1");
        let b_old = stored("b", Some("x"), &[], "u2");
        let b_new = stored("b", Some("y"), &[], "u2");
        let c_old = stored("c", None, &[], "u3");
        let c_new = stored("c", Some("y"), &[], "u3");
        let changes = Changes {
            category_changed: vec![(&a_old, &a_new), (&c_old, &c_new), (&b_old, &b_new)],
            ..Changes::default()
        };
        assert_eq!(
            render(&changes),
            [
                "【カテゴリ変更】\n<plain>(なし) → y</plain>\n:c:",
                "【カテゴリ変更】\n<plain>x → y</plain>\n:a: :b:",
            ]
        );
    }

    #[test]
    fn alias_changes_list_added_and_deleted_tags_and_skip_empty_diffs() {
        let a_old = stored("a", None, &["x", "y"], "u1");
        let a_new = stored("a", None, &["y", "z"], "u1");
        let b_old = stored("b", None, &["k"], "u2");
        let b_new = stored("b", None, &["k", ""], "u2");
        let c_old = stored("c", None, &[], "u3");
        let c_new = stored("c", None, &["n"], "u3");
        let changes = Changes {
            alias_changed: vec![(&a_old, &a_new), (&b_old, &b_new), (&c_old, &c_new)],
            ..Changes::default()
        };
        assert_eq!(
            render(&changes),
            ["【タグ変更】\n\
              $[x2 :a:] `:a:`\n追加: <plain>z</plain>\n削除: <plain>x</plain>\n\n\
              $[x2 :c:] `:c:`\n追加: <plain>n</plain>\n"]
        );
        let only_empty = Changes {
            alias_changed: vec![(&b_old, &b_new)],
            ..Changes::default()
        };
        assert!(render(&only_empty).is_empty());
    }

    /// 本文の見出し（1 行目）を除いた残り。
    fn body<'a>(text: &'a str, header: &str) -> &'a str {
        text.strip_prefix(header).unwrap()
    }

    #[test]
    fn pack_splits_under_the_limit_and_repeats_the_header() {
        let items: Vec<String> = ["aaaa", "bbbb", "cccc", "dddd", "eeee"]
            .map(str::to_owned)
            .to_vec();
        // 見出し 2 字 + 項目 4 字 + 区切り 2 字 + 項目 4 字 = 12 字まで。
        let texts = pack("H\n", &items, "\n\n", 12);
        assert_eq!(texts, ["H\naaaa\n\nbbbb", "H\ncccc\n\ndddd", "H\neeee"]);
        assert!(texts.iter().all(|t| note_length(t) <= 12));
        let joined: Vec<&str> = texts
            .iter()
            .flat_map(|t| body(t, "H\n").split("\n\n"))
            .collect();
        assert_eq!(joined, ["aaaa", "bbbb", "cccc", "dddd", "eeee"]);
    }

    #[test]
    fn pack_puts_an_item_over_the_limit_alone() {
        let items: Vec<String> = ["a", "long item", "b"].map(str::to_owned).to_vec();
        assert_eq!(
            pack("H\n", &items, " ", 5),
            ["H\na", "H\nlong item", "H\nb"]
        );
        assert!(pack("H\n", &[], " ", 5).is_empty());
    }

    #[test]
    fn long_changes_are_split_with_the_same_header_for_each_kind() {
        let added: Vec<StoredEmoji> = (0..30)
            .map(|i| stored(&format!("e{i:02}"), Some("c"), &[], "https://s/x"))
            .collect();
        let category_old: Vec<StoredEmoji> = (0..30)
            .map(|i| stored(&format!("e{i:02}"), Some("x"), &[], "u"))
            .collect();
        let category_new: Vec<StoredEmoji> = (0..30)
            .map(|i| stored(&format!("e{i:02}"), Some("y"), &[], "u"))
            .collect();
        let alias_old: Vec<StoredEmoji> = (0..30)
            .map(|i| stored(&format!("e{i:02}"), None, &[], "u"))
            .collect();
        let alias_new: Vec<StoredEmoji> = (0..30)
            .map(|i| stored(&format!("e{i:02}"), None, &["t"], "u"))
            .collect();
        let changes = Changes {
            added: added.iter().collect(),
            category_changed: category_old.iter().zip(&category_new).collect(),
            alias_changed: alias_old.iter().zip(&alias_new).collect(),
            ..Changes::default()
        };
        let limit = 150;
        let texts = render_limited(&changes, limit);
        assert!(texts.iter().all(|t| note_length(t) <= limit), "{texts:#?}");
        let count = |header: &str, sep: &str| -> usize {
            texts
                .iter()
                .filter_map(|t| t.strip_prefix(header))
                .map(|b| b.trim_end_matches('\n').split(sep).count())
                .sum()
        };
        assert_eq!(count("【絵文字追加】\n", "\n\n"), 30);
        assert_eq!(count("【カテゴリ変更】\n<plain>x → y</plain>\n", " "), 30);
        assert_eq!(count("【タグ変更】\n", "\n\n"), 30);
        assert!(
            texts
                .iter()
                .filter(|t| t.starts_with("【カテゴリ変更】"))
                .count()
                > 1
        );
    }
}
