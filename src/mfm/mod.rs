//! Misskey に投稿する本文（MFM）の組み立て。純粋関数にして、旧 C# 版の本文と文字列で厳密に比べる。

pub mod emoji;

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
