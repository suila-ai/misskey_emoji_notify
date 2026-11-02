//! 標準絵文字の画像の変更の通知文。

use url::Url;

use super::{MAX_NOTE_LENGTH, image_link, pack};
use crate::unicode_emoji::Change;

const HEADER: &str = "【標準絵文字変更】\n";
/// 一度に変わった絵文字がこれより多ければ、一覧を出さず件数だけを投稿する。絵文字セットごとの差し替え（約 3,800 件）で、
/// 160 を超えるノートを続けて投稿しないため。Twemoji の版上げで変わる既存の画像は多くても数十件なので、一覧で出る。
const MAX_LISTED: usize = 100;

/// 投稿する本文の列。ノートの上限を超えるときは、同じ見出しを付けて複数に分ける。[`MAX_LISTED`] より多く変わったときは、
/// 件数だけの 1 つにする。
///
/// 旧画像のリンクは出さない。差し替え前の URL は差し替え後の画像を返すので、リンク先が旧画像にならない。
pub fn render(changes: &[Change<'_>]) -> Vec<String> {
    if changes.len() > MAX_LISTED {
        return vec![format!(
            "{HEADER}{} 件の標準絵文字の画像が変わりました",
            changes.len()
        )];
    }
    let items: Vec<String> = changes
        .iter()
        .map(|change| {
            format!(
                "$[x2 {emoji}] <plain>{name}</plain>\n{link}",
                emoji = change.emoji.emoji,
                name = change.emoji.name,
                link = image_link("画像", &versioned(&change.new.actual_url, &change.new.sha1)),
            )
        })
        .collect();
    pack(HEADER, &items, "\n\n", MAX_NOTE_LENGTH)
}

/// `url` に `v={sha1 の先頭 8 桁}` を足す。差し替え前と同じ URL でも、ブラウザが 30 日キャッシュした旧画像でなく
/// 今の画像を開かせる。フラグメントがあれば、その前に入れる。URL として読めなければそのまま返す。
fn versioned(url: &str, sha1: &str) -> String {
    let Ok(mut url) = Url::parse(url) else {
        return url.to_owned();
    };
    url.query_pairs_mut()
        .append_pair("v", &sha1[..sha1.len().min(8)]);
    url.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unicode_emoji::test_support::{image, unicode};

    #[test]
    fn render_lists_the_emoji_its_name_and_a_versioned_link() {
        let grinning = unicode("😀", "grinning face");
        let heart = unicode("\u{2764}\u{fe0f}", "red heart");
        let new = image("https://s/twemoji/1f600.svg", "fedcba9876543210fedc");
        let heart_new = image("https://cdn/heart.png?size=72", "bbbbbbbbbbbb");
        let changes = [
            Change {
                emoji: &grinning,
                new: &new,
            },
            Change {
                emoji: &heart,
                new: &heart_new,
            },
        ];
        assert_eq!(
            render(&changes),
            ["【標準絵文字変更】\n\
              $[x2 😀] <plain>grinning face</plain>\n?[画像](https://s/twemoji/1f600.svg?v=fedcba98)\n\n\
              $[x2 \u{2764}\u{fe0f}] <plain>red heart</plain>\n?[画像](https://cdn/heart.png?size=72&v=bbbbbbbb)"]
        );
        assert!(render(&[]).is_empty());
    }

    #[test]
    fn versioned_puts_v_into_the_query_before_the_fragment() {
        assert_eq!(
            versioned("https://s/a.svg", "0123456789"),
            "https://s/a.svg?v=01234567"
        );
        assert_eq!(
            versioned("https://cdn/a.png?size=72", "0123456789"),
            "https://cdn/a.png?size=72&v=01234567"
        );
        assert_eq!(
            versioned("https://cdn/a.svg#frag", "0123456789"),
            "https://cdn/a.svg?v=01234567#frag"
        );
    }

    #[test]
    fn more_than_max_listed_changes_are_posted_as_a_count() {
        let grinning = unicode("😀", "grinning face");
        let new = image("https://s/twemoji/1f600.svg", "bbbb");
        let change = Change {
            emoji: &grinning,
            new: &new,
        };
        assert_eq!(
            render(&vec![change; MAX_LISTED + 1]),
            ["【標準絵文字変更】\n101 件の標準絵文字の画像が変わりました"]
        );
        let listed = render(&vec![change; MAX_LISTED]);
        assert!(listed.len() > 1, "{listed:?}");
        assert!(
            listed.iter().all(|text| text.contains("grinning face")),
            "{listed:?}"
        );
    }
}
