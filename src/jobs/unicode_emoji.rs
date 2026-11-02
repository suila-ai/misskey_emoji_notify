//! 標準絵文字の Twemoji の画像を確かめ、控えを保存してから、前回との差分を投稿するジョブ。
//!
//! 前回の控えが無ければ初回とみなし、保存するだけで投稿しない。取得に失敗した絵文字は前回の控えを引き継ぐので、
//! サーバが応答しない回は前回と同じになり、保存も投稿もしない。[`EmojiCheckJob`](super::EmojiCheckJob) と同じく、
//! 控えは投稿の前に保存し、投稿に失敗した変更は次回また投稿しない。画像ファイルは置かず、画像ごとの [`ImageInfo`](crate::emoji::ImageInfo) だけを控える。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing::{debug, info, warn};
use url::Url;

use super::{Error, Job, post_texts, spawn_blocking_in_span};
use crate::config::{UnicodeEmojiConfig, Visibility};
use crate::images::{self, ImageFetcher};
use crate::misskey::Misskey;
use crate::unicode_emoji::{self, Snapshot, UnicodeEmoji, diff};
use crate::{mfm, state};

pub struct UnicodeEmojiCheckJob<'a, M, F> {
    misskey: &'a M,
    fetcher: F,
    server: Url,
    visibility: Visibility,
    state_file: PathBuf,
    /// 確かめる絵文字。
    emojis: Vec<UnicodeEmoji>,
    /// 前回の控え。初回は `None`。保存と差し替えを 1 つの blocking タスクで行うので `Arc`。
    snapshot: Arc<Mutex<Option<Arc<Snapshot>>>>,
}

impl<'a, M: Misskey, F: ImageFetcher> UnicodeEmojiCheckJob<'a, M, F> {
    pub fn new(
        misskey: &'a M,
        fetcher: F,
        server: &Url,
        config: &UnicodeEmojiConfig,
        state_dir: &Path,
        snapshot: Option<Snapshot>,
        emojis: Vec<UnicodeEmoji>,
    ) -> Self {
        Self {
            misskey,
            fetcher,
            server: server.clone(),
            visibility: config.visibility,
            state_file: unicode_emoji::state_file(state_dir),
            emojis,
            snapshot: Arc::new(Mutex::new(snapshot.map(Arc::new))),
        }
    }

    /// 前回の控え。初回は `None`。
    fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot
            .lock()
            .expect("the snapshot lock is not poisoned")
            .clone()
    }

    /// 控えを保存し、前回の控えと差し替える。保存に失敗したら何も変えない。
    /// 保存と差し替えを別々に待つと、その間で timeout が掛かったときに、保存したのに差し替えず、
    /// 次の回に同じ差分をまた投稿しうる。そこで 1 つのクロージャにまとめる。
    async fn commit(&self, new: Snapshot) -> Result<Arc<Snapshot>, Error> {
        let state_file = self.state_file.clone();
        let shared = self.snapshot.clone();
        spawn_blocking_in_span(move || {
            state::save(&state_file, &new)?;
            let new = Arc::new(new);
            *shared.lock().expect("the snapshot lock is not poisoned") = Some(new.clone());
            Ok(new)
        })
        .await?
    }
}

impl<M: Misskey, F: ImageFetcher> Job for UnicodeEmojiCheckJob<'_, M, F> {
    fn name(&self) -> &'static str {
        "unicode_emoji_check"
    }

    async fn run(&self) -> Result<(), Error> {
        let old = self.snapshot();
        let targets = self
            .emojis
            .iter()
            .map(|emoji| {
                let previous = old.as_ref().and_then(|old| old.get(emoji.emoji).cloned());
                (
                    emoji.emoji,
                    unicode_emoji::twemoji_url(&self.server, emoji.emoji),
                    previous,
                )
            })
            .collect();
        let new: Snapshot = images::sync_urls(&self.fetcher, targets, None)
            .await
            .into_iter()
            .filter_map(|(emoji, image)| Some((emoji.to_owned(), image?)))
            .collect();
        debug!(
            emojis = self.emojis.len(),
            images = new.len(),
            previous = old.as_ref().map(|old| old.len()),
            "fetched the twemoji images"
        );
        // 前回の控えが無い 404 は warn にならないので、/twemoji を配らないサーバ（パスの変更やプロキシの設定）で
        // 一度も取れていないときは、ここで知らせる。
        if new.is_empty() {
            warn!(
                emojis = self.emojis.len(),
                "no twemoji image could be fetched"
            );
        }
        // 変わっていなければ、保存も投稿もしない。
        if old.as_deref() == Some(&new) {
            debug!("no change");
            return Ok(());
        }
        let new = self.commit(new).await?;
        let Some(old) = old else {
            info!(
                images = new.len(),
                "took the first snapshot; nothing is posted"
            );
            return Ok(());
        };
        let changes = diff(&self.emojis, &old, &new);
        if changes.is_empty() {
            debug!("no image change");
        } else {
            info!(changed = changes.len(), "changes found");
        }
        post_texts(
            self.misskey,
            self.visibility,
            &mfm::unicode_emoji::render(&changes),
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::images::fake::{FakeFetcher, Response};
    use crate::images::sha1_hex;
    use crate::misskey::fake::{Fail, FakeMisskey};
    use crate::state::test_support::TempDir;
    use crate::unicode_emoji::test_support::unicode;

    const SVG_A: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"1\"/></svg>";
    const SVG_B: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg>";
    const GRINNING: &str = "https://misskey.example.com/twemoji/1f600.svg";
    const HEART: &str = "https://misskey.example.com/twemoji/2764.svg";

    fn config() -> UnicodeEmojiConfig {
        "[misskey]\nserver = \"https://misskey.example.com\"\ntoken = \"t\"\n[emoji.unicode]\nvisibility = \"public\"\n"
            .parse::<Config>()
            .unwrap()
            .emoji
            .unicode
            .unwrap()
    }

    fn emojis() -> Vec<UnicodeEmoji> {
        vec![
            unicode("😀", "grinning face"),
            unicode("\u{2764}\u{fe0f}", "red heart"),
        ]
    }

    fn fetcher(grinning: Response, heart: Response) -> FakeFetcher {
        FakeFetcher::default()
            .with(GRINNING, grinning)
            .with(HEART, heart)
    }

    fn both(data: &[u8]) -> FakeFetcher {
        fetcher(
            Response::Image(data.to_vec(), None),
            Response::Image(data.to_vec(), None),
        )
    }

    /// 控えを状態ファイルから読んで作る。bot の起動と同じ。
    fn job<'a>(
        misskey: &'a FakeMisskey,
        fetcher: FakeFetcher,
        dir: &TempDir,
    ) -> UnicodeEmojiCheckJob<'a, FakeMisskey, FakeFetcher> {
        let snapshot = state::load(&unicode_emoji::state_file(dir.path())).unwrap();
        UnicodeEmojiCheckJob::new(
            misskey,
            fetcher,
            &"https://misskey.example.com".parse().unwrap(),
            &config(),
            dir.path(),
            snapshot,
            emojis(),
        )
    }

    fn saved(dir: &TempDir) -> Option<Snapshot> {
        state::load(&unicode_emoji::state_file(dir.path())).unwrap()
    }

    #[tokio::test]
    async fn the_first_run_takes_a_snapshot_without_files_and_posts_nothing() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        job(&misskey, both(SVG_A), &dir).run().await.unwrap();
        assert!(misskey.texts().is_empty());
        let snapshot = saved(&dir).unwrap();
        assert_eq!(snapshot.len(), 2);
        let grinning = &snapshot["😀"];
        assert_eq!(grinning.sha1, sha1_hex(SVG_A));
        assert_eq!(grinning.format, "svg");
        assert_eq!(grinning.actual_url, format!("{GRINNING}#actual"));
        // 画像ファイルは置かない。状態の置き場には控えだけ。
        let files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, ["unicode_emojis.json"]);
    }

    #[tokio::test]
    async fn a_changed_image_is_posted_and_so_is_the_change_back() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        job(&misskey, both(SVG_A), &dir).run().await.unwrap();
        job(
            &misskey,
            fetcher(
                Response::Image(SVG_B.to_vec(), None),
                Response::Image(SVG_A.to_vec(), None),
            ),
            &dir,
        )
        .run()
        .await
        .unwrap();
        let created = misskey.created();
        assert_eq!(created.len(), 1, "{created:?}");
        assert_eq!(
            created[0].note.text,
            format!(
                "【標準絵文字変更】\n$[x2 😀] <plain>grinning face</plain>\n?[画像]({GRINNING}?v={}#actual)",
                &sha1_hex(SVG_B)[..8]
            )
        );
        assert_eq!(created[0].visibility, Visibility::Public);
        assert_eq!(saved(&dir).unwrap()["😀"].sha1, sha1_hex(SVG_B));

        // 元に戻っても変更として投稿する。
        job(&misskey, both(SVG_A), &dir).run().await.unwrap();
        let texts = misskey.texts();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts[1].contains(&sha1_hex(SVG_A)[..8]), "{}", texts[1]);
    }

    #[tokio::test]
    async fn an_unchanged_snapshot_is_not_saved_again() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        let job = job(&misskey, both(SVG_A), &dir);
        job.run().await.unwrap();
        std::fs::remove_file(unicode_emoji::state_file(dir.path())).unwrap();
        job.run().await.unwrap();
        assert!(saved(&dir).is_none());
        assert!(misskey.texts().is_empty());
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_previous_image_and_posts_nothing() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        job(&misskey, both(SVG_A), &dir).run().await.unwrap();
        let before = saved(&dir).unwrap();
        // 全部失敗した回は前回と同じになり、保存も投稿もしない。
        let failing = job(&misskey, fetcher(Response::Fail, Response::Fail), &dir);
        std::fs::remove_file(unicode_emoji::state_file(dir.path())).unwrap();
        failing.run().await.unwrap();
        assert!(saved(&dir).is_none());
        assert!(misskey.texts().is_empty());
        state::save(&unicode_emoji::state_file(dir.path()), &before).unwrap();
        // 片方だけ失敗して片方が変わった回は、変わった方だけ投稿し、失敗した方は前回のまま。
        job(
            &misskey,
            fetcher(Response::Fail, Response::Image(SVG_B.to_vec(), None)),
            &dir,
        )
        .run()
        .await
        .unwrap();
        let texts = misskey.texts();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(texts[0].contains("red heart"), "{}", texts[0]);
        let after = saved(&dir).unwrap();
        assert_eq!(after["😀"], before["😀"]);
        assert_eq!(after["\u{2764}\u{fe0f}"].sha1, sha1_hex(SVG_B));
    }

    #[tokio::test]
    async fn a_failed_post_is_logged_and_the_snapshot_is_still_saved() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        job(&misskey, both(SVG_A), &dir).run().await.unwrap();
        misskey.set_failing(Some(Fail::Create));
        job(&misskey, both(SVG_B), &dir).run().await.unwrap();
        assert!(misskey.texts().is_empty());
        assert_eq!(saved(&dir).unwrap()["😀"].sha1, sha1_hex(SVG_B));
        // 次回は同じ変更を投稿しない。
        misskey.set_failing(None);
        job(&misskey, both(SVG_B), &dir).run().await.unwrap();
        assert!(misskey.texts().is_empty());
    }

    #[tokio::test]
    async fn a_newly_available_image_is_saved_but_not_posted() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        job(
            &misskey,
            fetcher(Response::Image(SVG_A.to_vec(), None), Response::Fail),
            &dir,
        )
        .run()
        .await
        .unwrap();
        assert_eq!(saved(&dir).unwrap().len(), 1);
        job(&misskey, both(SVG_A), &dir).run().await.unwrap();
        assert!(misskey.texts().is_empty());
        assert_eq!(saved(&dir).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_non_image_response_is_not_taken_as_a_change() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        job(&misskey, both(SVG_A), &dir).run().await.unwrap();
        let page = b"<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\"></html>";
        job(&misskey, both(page), &dir).run().await.unwrap();
        assert!(misskey.texts().is_empty());
        assert_eq!(saved(&dir).unwrap()["😀"].sha1, sha1_hex(SVG_A));
    }
}
