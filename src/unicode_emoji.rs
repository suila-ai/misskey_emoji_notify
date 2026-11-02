//! 標準絵文字（Unicode 絵文字）の一覧と、Misskey が配る Twemoji の画像の控え。
//!
//! Misskey の Web クライアントは標準絵文字を既定で Twemoji の画像で表示し、画像はサーバの `/twemoji/{コードポイント}.svg`
//! から取る。管理者がこのファイルの差し替えやリダイレクトで見た目を変えることがあるので、全絵文字の画像を取って
//! SHA-1 で比べる。一覧は `emojis` クレートの全絵文字（スキントーンの変種を含む）。サーバの Twemoji より新しい版の絵文字と、
//! Misskey の規則では Twemoji に無いファイル名になる絵文字（eye in speech bubble の `1f441-fe0f-200d-1f5e8-fe0f`）は
//! 404 になる。版で絞っても後者は残り、404 を許す扱い（[`sync_urls`](crate::images::sync_urls)）はどのみち要るので、絞らない。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use url::Url;

use crate::emoji::ImageInfo;

/// 標準絵文字 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnicodeEmoji {
    pub emoji: &'static str,
    /// 英語の名前（`grinning face` など）。通知に添える。
    pub name: &'static str,
}

/// 状態ファイルの名前。
pub fn state_file(state_dir: &Path) -> PathBuf {
    state_dir.join("unicode_emojis.json")
}

/// 全絵文字。スキントーンの変種も含む。
pub fn all() -> Vec<UnicodeEmoji> {
    emojis::iter()
        .flat_map(|emoji| match emoji.skin_tones() {
            Some(tones) => tones.collect(),
            None => vec![emoji],
        })
        .map(|emoji| UnicodeEmoji {
            emoji: emoji.as_str(),
            name: emoji.name(),
        })
        .collect()
}

/// `emoji` の Twemoji の画像の URL。Misskey の `char2twemojiFilePath` と同じ規則で、コードポイントの 16 進を `-` でつなぎ、
/// ZWJ（U+200D）を含まなければ VS16（U+FE0F）を除く。Misskey と同じく、サーバの URL のパスによらずルートの `/twemoji` から引く。
pub fn twemoji_url(server: &Url, emoji: &str) -> String {
    let mut codes: Vec<String> = emoji
        .chars()
        .map(|c| format!("{:x}", u32::from(c)))
        .collect();
    if !codes.iter().any(|code| code == "200d") {
        codes.retain(|code| code != "fe0f");
    }
    server
        .join(&format!("/twemoji/{}.svg", codes.join("-")))
        .expect("the twemoji path is a valid URL path")
        .into()
}

/// 控え。絵文字ごとの取れた画像。取れていない絵文字は入れない。
pub type Snapshot = BTreeMap<String, ImageInfo>;

/// 画像が変わった絵文字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change<'a> {
    pub emoji: &'a UnicodeEmoji,
    pub new: &'a ImageInfo,
}

/// `old` から `new` への差分。両方に画像があって SHA-1 が違う絵文字を、`emojis` の順に返す。
pub fn diff<'a>(emojis: &'a [UnicodeEmoji], old: &Snapshot, new: &'a Snapshot) -> Vec<Change<'a>> {
    emojis
        .iter()
        .filter_map(|emoji| {
            let (previous, current) = (old.get(emoji.emoji)?, new.get(emoji.emoji)?);
            (previous.sha1 != current.sha1).then_some(Change {
                emoji,
                new: current,
            })
        })
        .collect()
}

#[cfg(test)]
pub mod test_support {
    use super::*;

    pub fn unicode(emoji: &'static str, name: &'static str) -> UnicodeEmoji {
        UnicodeEmoji { emoji, name }
    }

    pub fn image(url: &str, sha1: &str) -> ImageInfo {
        ImageInfo {
            actual_url: url.to_owned(),
            sha1: sha1.to_owned(),
            format: "svg".to_owned(),
            etag: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::test_support::{image, unicode};
    use super::*;

    fn file(emoji: &str) -> String {
        let server: Url = "https://misskey.example.com".parse().unwrap();
        twemoji_url(&server, emoji)
            .strip_prefix("https://misskey.example.com/twemoji/")
            .unwrap()
            .to_owned()
    }

    #[test]
    fn twemoji_file_names_follow_misskey() {
        assert_eq!(file("😀"), "1f600.svg");
        // ZWJ が無ければ VS16 を除く。
        assert_eq!(file("\u{2764}\u{fe0f}"), "2764.svg");
        assert_eq!(file("#\u{fe0f}\u{20e3}"), "23-20e3.svg");
        // ZWJ があれば VS16 も残す（Misskey 自身も同じファイル名を引く）。
        assert_eq!(
            file("\u{1f441}\u{fe0f}\u{200d}\u{1f5e8}\u{fe0f}"),
            "1f441-fe0f-200d-1f5e8-fe0f.svg"
        );
        assert_eq!(file("\u{1f91e}\u{1f3fc}"), "1f91e-1f3fc.svg");
        assert_eq!(file("\u{1f1ef}\u{1f1f5}"), "1f1ef-1f1f5.svg");
    }

    #[test]
    fn twemoji_urls_start_at_the_server_root() {
        for server in [
            "https://misskey.example.com/sub",
            "https://misskey.example.com/sub/",
        ] {
            let server: Url = server.parse().unwrap();
            assert_eq!(
                twemoji_url(&server, "😀"),
                "https://misskey.example.com/twemoji/1f600.svg"
            );
        }
    }

    #[test]
    fn all_includes_skin_tones_without_duplicates() {
        let all = all();
        let emojis: HashSet<&str> = all.iter().map(|e| e.emoji).collect();
        assert_eq!(emojis.len(), all.len());
        assert!(all.len() > 3000, "{}", all.len());
        assert!(emojis.contains("😀"));
        assert!(emojis.contains("\u{1f91e}\u{1f3fc}"));
        let grinning = all.iter().find(|e| e.emoji == "😀").unwrap();
        assert_eq!(grinning.name, "grinning face");
    }

    #[test]
    fn diff_takes_only_emojis_whose_image_changed() {
        let emojis = [
            unicode("a", "a"),
            unicode("b", "b"),
            unicode("c", "c"),
            unicode("d", "d"),
        ];
        let old = Snapshot::from([
            ("a".to_owned(), image("u-a", "h1")),
            ("b".to_owned(), image("u-b", "h2")),
            ("c".to_owned(), image("u-c", "h3")),
        ]);
        let new = Snapshot::from([
            ("a".to_owned(), image("u-a", "h1-changed")),
            ("b".to_owned(), image("u-b2", "h2")),
            ("d".to_owned(), image("u-d", "h4")),
        ]);
        let changes = diff(&emojis, &old, &new);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].emoji.emoji, "a");
        assert_eq!(changes[0].new.sha1, "h1-changed");
    }
}
