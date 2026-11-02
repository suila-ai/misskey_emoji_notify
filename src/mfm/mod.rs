//! Misskey に投稿する本文（MFM）の組み立て。純粋関数にして、旧 C# 版の本文と文字列で厳密に比べる。

pub mod emoji;
pub mod mention;
pub mod ranking;

/// ノートの本文の上限。Misskey の `maxNoteTextLength` の既定で、接続先のサーバも同じ。
pub const MAX_NOTE_LENGTH: usize = 3000;

/// 本文の長さ。Misskey はコードポイントで数えるが、それより小さくならない UTF-16 の長さで数える。
pub fn note_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 画像へのリンク。`?[...]` はプレビューを出さないリンク。
fn image_link(text: &str, url: &str) -> String {
    format!("?[{text}]({url})")
}

/// カテゴリの表示。無ければ `(なし)`。
fn category(category: Option<&str>) -> &str {
    category.unwrap_or("(なし)")
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
