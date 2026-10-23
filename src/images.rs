//! 絵文字の画像の取得と保存。
//!
//! 確認のたびに全絵文字の画像を取る。管理者がリダイレクトで画像を差し替える運用があり、`url` が同じまま中身が変わるので、
//! URL の比較では検知できない。画像の変更は SHA-1 の違いで判定し、リダイレクト後の URL も控える。
//! 転送量を抑えるため、前回の `ETag` があれば `If-None-Match` を付け、`304` なら前回の控えを引き継ぐ。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{StreamExt, stream};
use sha1::{Digest, Sha1};
use tracing::{debug, warn};

use crate::emoji::{EmojiSet, ImageInfo, StoredEmoji};

/// 同時に取る画像の数。
const CONCURRENCY: usize = 8;
/// 1 回の同期で画像を取るのに使う時間の上限。過ぎた絵文字は前回の控えを引き継ぐ。
/// 取得元が応答しなくても、ジョブの timeout（30 分）より前に一覧の変更を保存して投稿できるようにする。
const SYNC_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// 画像 1 つの要求のタイムアウト。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 画像ファイルの置き場。
pub fn image_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("images")
}

/// 画像を 1 つ取る。テストではフェイクに差し替える。
pub trait ImageFetcher {
    /// `url` の画像を取る。`etag` があれば `If-None-Match` を付け、変わっていなければ [`Fetched::NotModified`]。
    async fn fetch(&self, url: &str, etag: Option<&str>) -> Result<Fetched, Error>;
}

/// 取った結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    NotModified,
    Image {
        /// リダイレクトを追った後の URL。
        actual_url: String,
        etag: Option<String>,
        data: Vec<u8>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request to {url} failed")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to write the image file {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// reqwest で取る実装。
#[derive(Debug, Clone)]
pub struct HttpImageFetcher {
    http: reqwest::Client,
}

impl HttpImageFetcher {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl ImageFetcher for HttpImageFetcher {
    async fn fetch(&self, url: &str, etag: Option<&str>) -> Result<Fetched, Error> {
        let http_error = |source| Error::Http {
            url: url.to_owned(),
            source,
        };
        let mut request = self.http.get(url).timeout(REQUEST_TIMEOUT);
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(http_error)?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified);
        }
        let response = response.error_for_status().map_err(http_error)?;
        let actual_url = response.url().to_string();
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let data = response.bytes().await.map_err(http_error)?.into();
        Ok(Fetched::Image {
            actual_url,
            etag,
            data,
        })
    }
}

/// `new` の全絵文字の画像を取り、`new` の各項目の `image` を埋める。
///
/// 前回の控え（`old` の同名か同 URL の項目）があれば、その `ETag` で条件付き要求にし、`304` なら前回の控えを引き継ぐ。
/// 取得に失敗した絵文字と、[`SYNC_DEADLINE`] までに取れなかった絵文字も前回の控えを引き継ぐ（無ければ `None` のまま）。
/// 画像ファイルは `dir` に `{sha1}.{ext}` で置き、同じ名前で同じ大きさのファイルがあれば書き直さない。
pub async fn sync<F: ImageFetcher>(fetcher: &F, old: &EmojiSet, new: &mut EmojiSet, dir: &Path) {
    let deadline = tokio::time::Instant::now() + SYNC_DEADLINE;
    let targets: Vec<(String, String, Option<ImageInfo>)> = new
        .iter()
        .map(|emoji| {
            let previous = old
                .get(&emoji.name)
                .or_else(|| old.get_by_url(&emoji.url))
                .and_then(|previous| previous.image.clone());
            (emoji.name.clone(), emoji.url.clone(), previous)
        })
        .collect();
    let results: Vec<(String, Option<ImageInfo>, bool)> = stream::iter(targets)
        .map(|(name, url, previous)| async move {
            let fetched =
                tokio::time::timeout_at(deadline, fetch_one(fetcher, &url, previous.clone(), dir))
                    .await;
            match fetched {
                Ok(image) => (name, image, false),
                Err(_) => (name, previous, true),
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    let timed_out = results
        .iter()
        .filter(|(_, _, timed_out)| *timed_out)
        .count();
    if timed_out > 0 {
        warn!(
            timed_out,
            deadline = ?SYNC_DEADLINE,
            "some images were not fetched in time; keeping the previous ones"
        );
    }
    for (name, image, _) in results {
        new.set_image(&name, image);
    }
}

async fn fetch_one<F: ImageFetcher>(
    fetcher: &F,
    url: &str,
    previous: Option<ImageInfo>,
    dir: &Path,
) -> Option<ImageInfo> {
    // 前回の画像ファイルが消えているか空なら、304 でも取り直す必要があるので条件を付けない。
    let etag = previous
        .as_ref()
        .filter(|image| has_content(&dir.join(image.file_name())))
        .and_then(|image| image.etag.as_deref());
    match fetcher.fetch(url, etag).await {
        Ok(Fetched::NotModified) => {
            debug!(url, "image not modified");
            previous
        }
        Ok(Fetched::Image {
            actual_url,
            etag,
            data,
        }) => {
            // 200 で返ったエラーページを画像の変更と取り違えないよう、画像でない応答は取得の失敗として扱う。
            let Some(format) = detect_format(&data) else {
                warn!(
                    url,
                    bytes = data.len(),
                    "the response is not an image; keeping the previous one"
                );
                return previous;
            };
            let image = ImageInfo {
                actual_url,
                sha1: sha1_hex(&data),
                format: format.to_owned(),
                etag,
            };
            let path = dir.join(image.file_name());
            if !is_saved(&path, data.len())
                && let Err(error) = write_file(&path, &data)
            {
                warn!(url, error = %crate::error_chain(&error), "failed to save the image; keeping the previous one");
                return previous;
            }
            Some(image)
        }
        Err(error) => {
            warn!(url, error = %crate::error_chain(&error), "failed to fetch the image; keeping the previous one");
            previous
        }
    }
}

/// `path` に `len` バイトのファイルがあるか。書きかけで切れたファイルは大きさが違うので、書き直す。
fn is_saved(path: &Path, len: usize) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.len() == len as u64)
}

/// `path` に空でないファイルがあるか。
fn has_content(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0)
}

/// 同じフォルダの一時ファイルに書いてから入れ替える。途中で止まっても、書きかけのファイルが `path` に残らない。
/// 同じ画像を使う絵文字は同時に取られるので、一時ファイルの名前は呼び出しごとに変える。残った一時ファイルは、
/// 次に状態を保存するときに [`remove_unreferenced`] が消す。
fn write_file(path: &Path, data: &[u8]) -> Result<(), Error> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_added_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = std::fs::write(&temp, data).and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(|source| Error::Write {
        path: path.to_owned(),
        source,
    })
}

/// `set` のどの絵文字からも参照されない画像ファイルを `dir` から消す。
pub fn remove_unreferenced(dir: &Path, set: &EmojiSet) {
    let referenced: std::collections::HashSet<String> =
        set.iter().filter_map(StoredEmoji::image_file).collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if referenced.contains(name) {
            continue;
        }
        if let Err(error) = std::fs::remove_file(entry.path()) {
            warn!(path = %entry.path().display(), error = %error, "failed to remove an unreferenced image");
        } else {
            debug!(name, "removed an unreferenced image");
        }
    }
}

/// `emoji` の画像ファイルのパス。画像を取っていなければ `None`。
pub fn image_path(dir: &Path, emoji: &StoredEmoji) -> Option<PathBuf> {
    Some(dir.join(emoji.image_file()?))
}

/// SHA-1 の 16 進。
pub fn sha1_hex(data: &[u8]) -> String {
    Sha1::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// マジックナンバーで画像の形式（拡張子）を判定する。画像でなければ `None`。
///
/// SVG も画像としない。Misskey は SVG を絵文字に登録できず（UNSUPPORTED_FILE_TYPE）、`<?xml` で始まる XHTML の
/// エラーページと見分けられないため。
fn detect_format(data: &[u8]) -> Option<&'static str> {
    image::guess_format(data)
        .ok()
        .map(|format| format.extensions_str()[0])
}

/// テストで使う、メモリ上の取得元。
#[cfg(test)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{Error, Fetched, ImageFetcher};

    /// URL ごとの応答。
    #[derive(Debug, Clone)]
    pub enum Response {
        /// 画像の中身と、応答の ETag。リダイレクト後の URL は `url` に `#actual` を足したもの。
        Image(Vec<u8>, Option<&'static str>),
        /// `If-None-Match` が来れば 304、来なければ画像。
        Cached(Vec<u8>, &'static str),
        Fail,
        /// 応答を返さない。
        Hang,
    }

    #[derive(Debug, Default)]
    pub struct FakeFetcher {
        responses: HashMap<String, Response>,
        /// 呼ばれた（URL、送った ETag）。
        pub calls: Mutex<Vec<(String, Option<String>)>>,
    }

    impl FakeFetcher {
        pub fn with(mut self, url: &str, response: Response) -> Self {
            self.responses.insert(url.to_owned(), response);
            self
        }
    }

    impl ImageFetcher for FakeFetcher {
        async fn fetch(&self, url: &str, etag: Option<&str>) -> Result<Fetched, Error> {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_owned(), etag.map(str::to_owned)));
            let image = |data: &Vec<u8>, etag: Option<&str>| Fetched::Image {
                actual_url: format!("{url}#actual"),
                etag: etag.map(str::to_owned),
                data: data.clone(),
            };
            match self.responses.get(url) {
                Some(Response::Image(data, tag)) => Ok(image(data, *tag)),
                Some(Response::Cached(data, tag)) => {
                    if etag == Some(tag) {
                        Ok(Fetched::NotModified)
                    } else {
                        Ok(image(data, Some(tag)))
                    }
                }
                Some(Response::Hang) => std::future::pending().await,
                Some(Response::Fail) | None => Err(Error::Http {
                    url: url.to_owned(),
                    source: reqwest::get("http://[::1]:1").await.unwrap_err(),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeFetcher, Response};
    use super::*;
    use crate::emoji::test_support::{stored, with_image};
    use crate::state::test_support::TempDir;

    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
    const GIF: &[u8] = b"GIF89a....";

    #[test]
    fn formats_are_detected_by_magic_numbers() {
        assert_eq!(detect_format(PNG), Some("png"));
        assert_eq!(detect_format(GIF), Some("gif"));
        assert_eq!(detect_format(&[0xFF, 0xD8, 0xFF]), Some("jpg"));
        assert_eq!(detect_format(b"RIFF....WEBPVP8 "), Some("webp"));
        assert_eq!(detect_format(b"\0\0\0\x1cftypavif"), Some("avif"));
        assert_eq!(
            detect_format(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            None
        );
        assert_eq!(detect_format(b"hello"), None);
        assert_eq!(detect_format(b""), None);
    }

    #[test]
    fn sha1_is_lower_hex() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[tokio::test]
    async fn sync_fetches_saves_and_uses_etags() {
        let dir = TempDir::new();
        let fetcher = FakeFetcher::default()
            .with("u-a", Response::Image(PNG.to_vec(), Some("\"tag-a\"")))
            .with("u-b", Response::Cached(GIF.to_vec(), "\"tag-b\""))
            .with("u-c", Response::Fail);
        let old = EmojiSet::from_stored([
            // 前回取れていて、ファイルも残っている。
            {
                let mut e = with_image(stored("b", None, &[], "u-b"), &sha1_hex(GIF));
                let image = e.image.as_mut().unwrap();
                image.format = "gif".to_owned();
                image.etag = Some("\"tag-b\"".to_owned());
                std::fs::write(dir.join(format!("{}.gif", image.sha1)), GIF).unwrap();
                e
            },
            with_image(stored("c", None, &[], "u-c"), "h-c"),
        ]);
        let mut new = EmojiSet::from_stored([
            stored("a", None, &[], "u-a"),
            stored("b", None, &[], "u-b"),
            stored("c", None, &[], "u-c"),
        ]);
        sync(&fetcher, &old, &mut new, dir.path()).await;

        let a = new.get("a").unwrap().image.clone().unwrap();
        assert_eq!(a.sha1, sha1_hex(PNG));
        assert_eq!(a.format, "png");
        assert_eq!(a.actual_url, "u-a#actual");
        assert_eq!(a.etag.as_deref(), Some("\"tag-a\""));
        assert_eq!(
            std::fs::read(dir.join(format!("{}.png", a.sha1))).unwrap(),
            PNG
        );
        // 304 なら前回の控えのまま。
        assert_eq!(new.get("b").unwrap().image, old.get("b").unwrap().image);
        // 失敗したら前回の控えのまま。
        assert_eq!(new.get("c").unwrap().image, old.get("c").unwrap().image);

        let mut calls = fetcher.calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(
            calls,
            [
                ("u-a".to_owned(), None),
                ("u-b".to_owned(), Some("\"tag-b\"".to_owned())),
                ("u-c".to_owned(), None),
            ]
        );
    }

    #[tokio::test]
    async fn sync_does_not_send_the_etag_when_the_file_is_missing() {
        let dir = TempDir::new();
        let fetcher = FakeFetcher::default().with("u", Response::Cached(GIF.to_vec(), "\"tag\""));
        let old = EmojiSet::from_stored([{
            let mut e = with_image(stored("a", None, &[], "u"), "stale");
            e.image.as_mut().unwrap().etag = Some("\"tag\"".to_owned());
            e
        }]);
        let mut new = EmojiSet::from_stored([stored("a", None, &[], "u")]);
        sync(&fetcher, &old, &mut new, dir.path()).await;
        assert_eq!(fetcher.calls.lock().unwrap()[0].1, None);
        assert_eq!(
            new.get("a").unwrap().image.as_ref().unwrap().sha1,
            sha1_hex(GIF)
        );
    }

    #[tokio::test]
    async fn sync_uses_the_previous_entry_by_url_when_renamed() {
        let dir = TempDir::new();
        let fetcher = FakeFetcher::default().with("u", Response::Cached(GIF.to_vec(), "\"tag\""));
        let old = EmojiSet::from_stored([{
            let mut e = with_image(stored("old", None, &[], "u"), &sha1_hex(GIF));
            let image = e.image.as_mut().unwrap();
            image.format = "gif".to_owned();
            image.etag = Some("\"tag\"".to_owned());
            std::fs::write(dir.join(format!("{}.gif", image.sha1)), GIF).unwrap();
            e
        }]);
        let mut new = EmojiSet::from_stored([stored("new", None, &[], "u")]);
        sync(&fetcher, &old, &mut new, dir.path()).await;
        assert_eq!(new.get("new").unwrap().image, old.get("old").unwrap().image);
    }

    #[tokio::test]
    async fn a_truncated_file_is_written_again() {
        let dir = TempDir::new();
        let fetcher = FakeFetcher::default().with("u", Response::Image(PNG.to_vec(), None));
        let path = dir.join(format!("{}.png", sha1_hex(PNG)));
        // 書き込みが途中で止まったファイル。
        std::fs::write(&path, &PNG[..3]).unwrap();
        let old = EmojiSet::from_stored([{
            let mut e = with_image(stored("a", None, &[], "u"), &sha1_hex(PNG));
            e.image.as_mut().unwrap().format = "png".to_owned();
            e
        }]);
        let mut new = EmojiSet::from_stored([stored("a", None, &[], "u")]);
        sync(&fetcher, &old, &mut new, dir.path()).await;
        assert_eq!(std::fs::read(&path).unwrap(), PNG);
    }

    #[tokio::test]
    async fn an_empty_file_is_fetched_again_without_the_etag() {
        let dir = TempDir::new();
        let fetcher = FakeFetcher::default().with("u", Response::Cached(GIF.to_vec(), "\"tag\""));
        let path = dir.join(format!("{}.gif", sha1_hex(GIF)));
        std::fs::write(&path, b"").unwrap();
        let old = EmojiSet::from_stored([{
            let mut e = with_image(stored("a", None, &[], "u"), &sha1_hex(GIF));
            let image = e.image.as_mut().unwrap();
            image.format = "gif".to_owned();
            image.etag = Some("\"tag\"".to_owned());
            e
        }]);
        let mut new = EmojiSet::from_stored([stored("a", None, &[], "u")]);
        sync(&fetcher, &old, &mut new, dir.path()).await;
        assert_eq!(fetcher.calls.lock().unwrap()[0].1, None);
        assert_eq!(std::fs::read(&path).unwrap(), GIF);
    }

    #[tokio::test]
    async fn emojis_sharing_a_url_both_get_the_image() {
        let dir = TempDir::new();
        let fetcher = FakeFetcher::default().with("u", Response::Image(PNG.to_vec(), None));
        let mut new =
            EmojiSet::from_stored([stored("a", None, &[], "u"), stored("b", None, &[], "u")]);
        sync(&fetcher, &EmojiSet::default(), &mut new, dir.path()).await;
        for name in ["a", "b"] {
            let image = new.get(name).unwrap().image.clone().unwrap();
            assert_eq!(image.sha1, sha1_hex(PNG), "{name}");
        }
        let files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, [format!("{}.png", sha1_hex(PNG))]);
    }

    #[test]
    fn unreferenced_files_are_removed() {
        let dir = TempDir::new();
        std::fs::write(dir.join("keep.png"), b"x").unwrap();
        std::fs::write(dir.join("drop.png"), b"x").unwrap();
        let set = EmojiSet::from_stored([{
            let mut e = with_image(stored("a", None, &[], "u"), "keep");
            e.image.as_mut().unwrap().format = "png".to_owned();
            e
        }]);
        remove_unreferenced(dir.path(), &set);
        assert!(dir.join("keep.png").exists());
        assert!(!dir.join("drop.png").exists());
        assert_eq!(
            image_path(dir.path(), set.get("a").unwrap()).unwrap(),
            dir.join("keep.png")
        );
    }
}
