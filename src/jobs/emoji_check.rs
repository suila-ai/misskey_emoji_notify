//! 絵文字一覧を確かめ、状態を保存してから、前回との差分を投稿し、バナーを一覧に合わせるジョブ。
//!
//! 前回の状態（共有の [`EmojiSet`]）が無ければ初回とみなし、保存するだけで投稿しない。バナーは初回から作る。
//! 空の一覧が返ったら（前回も空のときを除く）、サーバの不調かもしれないので 1 回だけ見送り、続いたら受け入れる。
//! 初回も見送らないと、一時的な空の一覧を控えて、次の回に全絵文字を追加として投稿する。
//! 重複投稿より取りこぼしを選ぶ。状態は投稿の前に保存し、保存に失敗したら投稿せずに次の回で同じ差分を試す。
//! 保存のあとで止まると（timeout や強制終了）、残りの投稿は抜ける。投稿に失敗した種別も error ログを出して続け、
//! 同じ変更を次回また投稿はしない。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, error, info, warn};

use super::{Error, Job};
use crate::SharedEmojiSet;
use crate::config::{EmojiConfig, Visibility};
use crate::emoji::{self, EmojiSet, diff};
use crate::images::{self, ImageFetcher};
use crate::misskey::{BannerFile, Misskey, NewNote};
use crate::{banner, mfm, state};

/// bot が上げたバナーのファイル名の接頭辞。
const BANNER_PREFIX: &str = "banner_";
/// バナーのファイル名に入れる指紋の桁数（16 進）。
const FINGERPRINT_LEN: usize = 16;
/// バナーの指紋の版。並べ方を変えたら上げて、どのサーバでも 1 回作り直させる。
const BANNER_VERSION: &str = "v3";

/// `set` から作るバナーのファイル名。画像のある絵文字の SHA-1 を名前の順に並べた指紋を含むので、
/// 今のバナーの名前と比べれば、バナーが一覧に合っているかが分かる。
fn banner_name(set: &EmojiSet) -> String {
    let mut fingerprint = String::from(BANNER_VERSION);
    for image in set.iter().filter_map(|emoji| emoji.image.as_ref()) {
        fingerprint.push('\n');
        fingerprint.push_str(&image.sha1);
    }
    let fingerprint = images::sha1_hex(fingerprint.as_bytes());
    format!("{BANNER_PREFIX}{}.png", &fingerprint[..FINGERPRINT_LEN])
}

/// bot が上げたバナーの名前か。[`banner_name`] の形（接頭辞、指紋の小文字の 16 進、`.png`）のものだけを bot のものとみなし、
/// 所有者が手で上げた `banner_*.png` は消さない。
fn is_bot_banner(name: &str) -> bool {
    name.strip_prefix(BANNER_PREFIX)
        .and_then(|rest| rest.strip_suffix(".png"))
        .is_some_and(|fingerprint| {
            fingerprint.len() == FINGERPRINT_LEN
                && fingerprint
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// [`EmojiCheckJob::commit`] の本体。保存できたら共有の一覧を差し替え、`image_dir` があれば参照されない画像を消す。
fn save_and_swap(
    state_file: &Path,
    shared: &SharedEmojiSet,
    image_dir: Option<&Path>,
    set: EmojiSet,
) -> Result<Arc<EmojiSet>, Error> {
    state::save(state_file, &set.iter().collect::<Vec<_>>())?;
    let set = Arc::new(set);
    *shared.write().expect("the emoji set lock is not poisoned") = Some(set.clone());
    if let Some(dir) = image_dir {
        images::remove_unreferenced(dir, &set);
    }
    Ok(set)
}

/// 今の span の中で、`f` を別スレッドで動かす。
fn spawn_blocking_in_span<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || span.in_scope(f))
}

pub struct EmojiCheckJob<'a, M, F> {
    misskey: &'a M,
    fetcher: F,
    /// 通知の公開範囲。通知しないなら `None`。
    notify: Option<Visibility>,
    update_banner: bool,
    state_file: PathBuf,
    image_dir: PathBuf,
    shared: SharedEmojiSet,
    /// 直前の回で、空の一覧を見送ったか。
    skipped_empty: AtomicBool,
    /// デコードできる画像が 1 つも無くて作れなかったバナーの名前。一覧が変わって名前が変わるまで作り直さない。
    unbuildable_banner: Mutex<Option<String>>,
}

impl<'a, M: Misskey, F: ImageFetcher> EmojiCheckJob<'a, M, F> {
    pub fn new(misskey: &'a M, fetcher: F, config: &EmojiConfig, shared: SharedEmojiSet) -> Self {
        Self {
            misskey,
            fetcher,
            notify: config.notify.as_ref().map(|notify| notify.visibility),
            update_banner: config.banner.is_some(),
            state_file: emoji::state_file(&config.state_dir),
            image_dir: images::image_dir(&config.state_dir),
            shared,
            skipped_empty: AtomicBool::new(false),
            unbuildable_banner: Mutex::new(None),
        }
    }

    /// 絵文字の画像が要るか。通知（画像変更の検知）とバナーのとき。
    fn fetch_images(&self) -> bool {
        self.notify.is_some() || self.update_banner
    }

    /// 前回の一覧。初回は `None`。
    fn snapshot(&self) -> Option<Arc<EmojiSet>> {
        self.shared
            .read()
            .expect("the emoji set lock is not poisoned")
            .clone()
    }

    /// 状態を保存し、共有の一覧を差し替え、参照されない画像を消す。保存に失敗したら何も変えない。
    ///
    /// ファイルの読み書きは別スレッドで行う。保存と差し替えを別々に待つと、その間で timeout が掛かったときに、
    /// 保存したのに差し替えず、次の回に同じ差分をまた投稿しうる。そこで 1 つのクロージャにまとめる。
    /// 待っている間に timeout で止まっても、クロージャは最後まで動く。
    async fn commit(&self, set: EmojiSet) -> Result<Arc<EmojiSet>, Error> {
        let state_file = self.state_file.clone();
        let shared = self.shared.clone();
        let image_dir = self.fetch_images().then(|| self.image_dir.clone());
        spawn_blocking_in_span(move || {
            save_and_swap(&state_file, &shared, image_dir.as_deref(), set)
        })
        .await?
    }

    /// `old` から `new` への差分を、状態を保存してから投稿する。保存した一覧を返す。
    async fn post_diff(&self, old: &EmojiSet, new: EmojiSet) -> Result<Arc<EmojiSet>, Error> {
        // 差分は `new` を借りるので、保存して手放す前に本文を組み立てておく。
        let posts = {
            let changes = diff(old, &new);
            if changes.is_empty() {
                debug!("no change");
            } else {
                info!(
                    added = changes.added.len(),
                    deleted = changes.deleted.len(),
                    renamed = changes.renamed.len(),
                    image_changed = changes.image_changed.len(),
                    category_changed = changes.category_changed.len(),
                    alias_changed = changes.alias_changed.len(),
                    "changes found"
                );
            }
            self.notify
                .map(|visibility| (visibility, mfm::emoji::render(&changes)))
        };
        let saved = self.commit(new).await?;
        if let Some((visibility, texts)) = posts {
            self.post_changes(visibility, &texts).await;
        }
        Ok(saved)
    }

    async fn post_changes(&self, visibility: Visibility, texts: &[String]) {
        for text in texts {
            match self.misskey.create_note(NewNote { text, visibility }).await {
                Ok(note) => info!(note_id = %note.id, "posted"),
                Err(error) => {
                    error!(error = %crate::error_chain(&error), length = mfm::note_length(text), "failed to post a change; it will not be retried")
                }
            }
        }
    }

    /// バナーを今の絵文字一覧に合わせる。今のバナーの名前が一覧から決まる名前と違えば作り直す。
    /// 初回、絵文字の増減や画像の変更、誰かがバナーを差し替えたり消したりしたときに作り直すことになる。
    /// 一覧が空なら、消えた絵文字を並べたまま残さないよう bot のバナーを外す。所有者のバナーは残す。
    /// デコードできる画像が 1 つも無くて作れなかった一覧は、変わるまで作り直さない（確認のたびに全画像を読んで warn を重ねないため）。
    /// 失敗は warn で済ませ、次の回にまた試す。
    async fn keep_banner(&self, set: Arc<EmojiSet>) {
        let current = match self.misskey.current_banner().await {
            Ok(current) => current,
            Err(error) => {
                warn!(error = %crate::error_chain(&error), "failed to read the current banner");
                return;
            }
        };
        if set.is_empty() {
            if let Some(current) = current.filter(|current| is_bot_banner(&current.name)) {
                self.remove_banner(current).await;
            }
            return;
        }
        let name = banner_name(&set);
        if current.as_ref().is_some_and(|current| current.name == name) {
            debug!(name, "the banner matches the emoji list");
            return;
        }
        if self
            .unbuildable_banner
            .lock()
            .expect("the banner lock is not poisoned")
            .as_deref()
            == Some(name.as_str())
        {
            debug!(name, "the banner cannot be built from this emoji list");
            return;
        }
        let dir = self.image_dir.clone();
        let built = spawn_blocking_in_span(move || {
            let paths: Vec<PathBuf> = set
                .iter()
                .filter_map(|emoji| images::image_path(&dir, emoji))
                .collect();
            banner::build(&paths)
        });
        let png = match built.await {
            Ok(Some(png)) => png,
            Ok(None) => {
                warn!("no emoji image could be used; the banner is left as it is");
                *self
                    .unbuildable_banner
                    .lock()
                    .expect("the banner lock is not poisoned") = Some(name);
                return;
            }
            Err(error) => {
                warn!(error = %error, "building the banner panicked; the banner is left as it is");
                return;
            }
        };
        if let Err(error) = self.upload_banner(&name, png, current).await {
            warn!(error = %crate::error_chain(&error), "failed to replace the banner");
        }
    }

    /// `png` を `name` で上げてバナーにし、bot が上げた前のバナー（`previous`）を消す。
    async fn upload_banner(
        &self,
        name: &str,
        png: Vec<u8>,
        previous: Option<BannerFile>,
    ) -> Result<(), crate::misskey::Error> {
        let file_id = self.misskey.upload_file(name, "image/png", png).await?;
        if let Err(error) = self.misskey.set_banner(Some(&file_id)).await {
            if let Err(error) = self.misskey.delete_file(&file_id).await {
                warn!(file_id, error = %crate::error_chain(&error), "failed to delete the banner that could not be set");
            }
            return Err(error);
        }
        info!(file_id, "banner replaced");
        if let Some(previous) = previous
            && is_bot_banner(&previous.name)
            && let Err(error) = self.misskey.delete_file(&previous.id).await
        {
            warn!(file_id = %previous.id, error = %crate::error_chain(&error), "failed to delete the previous banner");
        }
        Ok(())
    }

    /// bot のバナー `current` を外して消す。失敗は warn で済ませ、次の回にまた試す。
    async fn remove_banner(&self, current: BannerFile) {
        if let Err(error) = self.misskey.set_banner(None).await {
            warn!(error = %crate::error_chain(&error), "failed to remove the banner");
            return;
        }
        info!(file_id = %current.id, "banner removed as no emoji is left");
        if let Err(error) = self.misskey.delete_file(&current.id).await {
            warn!(file_id = %current.id, error = %crate::error_chain(&error), "failed to delete the removed banner");
        }
    }
}

impl<M: Misskey, F: ImageFetcher> Job for EmojiCheckJob<'_, M, F> {
    fn name(&self) -> &'static str {
        "emoji_check"
    }

    async fn run(&self) -> Result<(), Error> {
        let emojis = self.misskey.emojis().await?;
        let mut new = EmojiSet::from_api(emojis);
        let old = self.snapshot();
        debug!(
            emojis = new.len(),
            previous = old.as_ref().map(|old| old.len()),
            "fetched the emoji list"
        );
        let suspicious = new.is_empty() && old.as_ref().is_none_or(|old| !old.is_empty());
        if suspicious && !self.skipped_empty.swap(true, Ordering::Relaxed) {
            warn!(
                "the emoji list came back empty; skipping this check in case the server is having trouble"
            );
            return Ok(());
        }
        self.skipped_empty.store(false, Ordering::Relaxed);
        if self.fetch_images() {
            state::create_dir(&self.image_dir)?;
            let previous = old.clone().unwrap_or_default();
            images::sync(&self.fetcher, &previous, &mut new, &self.image_dir).await;
        }
        let saved = match old {
            None => {
                info!(
                    emojis = new.len(),
                    "took the first snapshot; nothing is posted"
                );
                self.commit(new).await?
            }
            // 変わっていなければ、保存も投稿もしない。
            Some(old) if *old == new => {
                debug!("no change");
                old
            }
            Some(old) => self.post_diff(&old, new).await?,
        };
        if self.update_banner {
            self.keep_banner(saved).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::images::fake::{FakeFetcher, Response};
    use crate::misskey::Emoji;
    use crate::misskey::fake::{Fail, FakeMisskey};
    use crate::state::test_support::TempDir;

    const PNG: &[u8] = include_bytes!("../../tests/fixtures/images/red.png");
    const BLUE: &[u8] = include_bytes!("../../tests/fixtures/images/blue.png");

    fn emoji(name: &str, category: Option<&str>, aliases: &[&str], url: &str) -> Emoji {
        Emoji {
            name: name.to_owned(),
            category: category.map(str::to_owned),
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
            url: url.to_owned(),
        }
    }

    fn config(dir: &TempDir, sections: &str) -> EmojiConfig {
        format!(
            "[misskey]\nserver = \"https://misskey.example.com\"\ntoken = \"t\"\n[emoji]\nstate_dir = {:?}\n{sections}",
            dir.path().to_str().unwrap()
        )
        .parse::<Config>()
        .unwrap()
        .emoji
    }

    fn shared() -> SharedEmojiSet {
        SharedEmojiSet::default()
    }

    fn fetcher() -> FakeFetcher {
        FakeFetcher::default()
            .with("u-a", Response::Image(PNG.to_vec(), None))
            .with("u-b", Response::Image(b"GIF89a..".to_vec(), None))
            .with("u-c", Response::Image(PNG.to_vec(), None))
    }

    #[tokio::test]
    async fn the_first_run_posts_nothing_and_makes_the_banner() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let shared = shared();
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n[emoji.banner]\n"),
            shared.clone(),
        );
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
        assert!(misskey.current_banner().await.unwrap().is_some());
        assert_eq!(misskey.uploads(), 1);
        let saved: Vec<emoji::StoredEmoji> =
            state::load(&dir.join("emojis.json")).unwrap().unwrap();
        assert_eq!(saved.len(), 1);
        assert!(
            saved[0].image.is_some(),
            "images are fetched even on the first run"
        );
        assert_eq!(shared.read().unwrap().as_ref().unwrap().len(), 1);
        assert!(
            dir.join("images")
                .join(saved[0].image_file().unwrap())
                .exists()
        );
    }

    #[tokio::test]
    async fn the_second_run_posts_the_changes_and_replaces_the_banner() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new()
            .with_emojis(vec![
                emoji("a", Some("c"), &["x"], "u-a"),
                emoji("b", None, &[], "u-b"),
            ])
            .with_banner("old-banner", "banner_0123456789abcdef.png");
        let shared = shared();
        let config = config(
            &dir,
            "[emoji.notify]\nvisibility = \"public\"\n[emoji.banner]\n",
        );
        let job = EmojiCheckJob::new(&misskey, fetcher(), &config, shared.clone());
        job.run().await.unwrap();

        misskey.set_emojis(vec![
            emoji("a", Some("d"), &["x", "y"], "u-a"),
            emoji("c", None, &[], "u-c"),
        ]);
        job.run().await.unwrap();

        let created = misskey.created();
        let texts: Vec<&str> = created.iter().map(|c| c.note.text.as_str()).collect();
        assert_eq!(texts.len(), 4, "{texts:?}");
        assert!(
            texts[0].starts_with("【絵文字追加】\n$[x2 :c:]"),
            "{}",
            texts[0]
        );
        assert!(
            texts[1].starts_with("【絵文字削除】\n`:b:`"),
            "{}",
            texts[1]
        );
        assert_eq!(texts[2], "【カテゴリ変更】\n<plain>c → d</plain>\n:a:");
        assert_eq!(
            texts[3],
            "【タグ変更】\n$[x2 :a:] `:a:`\n追加: <plain>y</plain>\n"
        );
        assert!(created.iter().all(|c| c.visibility == Visibility::Public));
        // バナーは新しいファイルになり、前のものは消える。
        let banner = misskey.banner_id().unwrap();
        assert_ne!(banner, "old-banner");
        assert_eq!(misskey.file_names().len(), 1);
        assert!(misskey.file_names()[0].starts_with("banner_"));
        // 状態は新しい一覧になり、消えた絵文字の画像ファイルは残らない。
        assert_eq!(shared.read().unwrap().as_ref().unwrap().get("b"), None);
        let gif = images::sha1_hex(b"GIF89a..");
        assert!(!dir.join("images").join(format!("{gif}.gif")).exists());
    }

    #[tokio::test]
    async fn an_unchanged_list_is_not_saved_again() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared(),
        );
        job.run().await.unwrap();
        std::fs::remove_file(dir.join("emojis.json")).unwrap();
        job.run().await.unwrap();
        assert!(!dir.join("emojis.json").exists());
    }

    #[tokio::test]
    async fn a_failed_post_is_logged_and_the_state_is_still_saved() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let shared = shared();
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared.clone(),
        );
        job.run().await.unwrap();
        misskey.set_emojis(vec![
            emoji("a", None, &[], "u-a"),
            emoji("b", None, &[], "u-b"),
        ]);
        misskey.set_failing(Some(Fail::Create));
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
        assert_eq!(shared.read().unwrap().as_ref().unwrap().len(), 2);
        // 次回は同じ変更を投稿しない。
        misskey.set_failing(None);
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
    }

    #[tokio::test]
    async fn an_error_page_is_not_taken_as_an_image_change() {
        let pages: [&[u8]; 2] = [
            b"<!DOCTYPE html><html><body>maintenance</body></html>",
            b"<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\"></html>",
        ];
        for page in pages {
            let dir = TempDir::new();
            let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
            let shared = shared();
            let config = config(&dir, "[emoji.notify]\n");
            let ok = || FakeFetcher::default().with("u-a", Response::Image(PNG.to_vec(), None));
            let error_page =
                FakeFetcher::default().with("u-a", Response::Image(page.to_vec(), None));
            EmojiCheckJob::new(&misskey, ok(), &config, shared.clone())
                .run()
                .await
                .unwrap();
            let before = shared
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .get("a")
                .unwrap()
                .image
                .clone();
            // エラーページが返った回も、元に戻った回も、画像は前のままで投稿しない。
            EmojiCheckJob::new(&misskey, error_page, &config, shared.clone())
                .run()
                .await
                .unwrap();
            assert_eq!(
                shared
                    .read()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .get("a")
                    .unwrap()
                    .image,
                before
            );
            EmojiCheckJob::new(&misskey, ok(), &config, shared.clone())
                .run()
                .await
                .unwrap();
            assert!(misskey.texts().is_empty(), "{:?}", misskey.texts());
        }
    }

    #[tokio::test]
    async fn the_first_addition_on_a_server_without_emojis_is_posted() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared(),
        );
        // 初回の空の一覧も 1 回は見送り、2 回続いたら空の一覧を控える。
        job.run().await.unwrap();
        assert!(
            state::load::<Vec<emoji::StoredEmoji>>(&dir.join("emojis.json"))
                .unwrap()
                .is_none()
        );
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
        let saved: Vec<emoji::StoredEmoji> =
            state::load(&dir.join("emojis.json")).unwrap().unwrap();
        assert!(saved.is_empty());
        // 控えがあるので、空からの追加も投稿する。
        misskey.set_emojis(vec![emoji("a", None, &[], "u-a")]);
        job.run().await.unwrap();
        let texts = misskey.texts();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].starts_with("【絵文字追加】"), "{}", texts[0]);
    }

    #[tokio::test]
    async fn a_passing_empty_list_on_the_first_run_is_not_saved() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new();
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared(),
        );
        job.run().await.unwrap();
        // 一覧が戻っても、初回として控えるだけで、全部を追加として投稿しない。
        misskey.set_emojis(vec![emoji("a", None, &[], "u-a")]);
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
        let saved: Vec<emoji::StoredEmoji> =
            state::load(&dir.join("emojis.json")).unwrap().unwrap();
        assert_eq!(saved.len(), 1);
    }

    #[tokio::test]
    async fn an_empty_list_is_accepted_only_when_it_comes_twice_in_a_row() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let shared = shared();
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared.clone(),
        );
        job.run().await.unwrap();
        let image = dir
            .join("images")
            .join(format!("{}.png", images::sha1_hex(PNG)));
        assert!(image.exists());

        // 1 回だけの空の一覧は見送る。状態も画像も変えない。
        misskey.set_emojis(Vec::new());
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
        assert_eq!(shared.read().unwrap().as_ref().unwrap().len(), 1);
        let saved: Vec<emoji::StoredEmoji> =
            state::load(&dir.join("emojis.json")).unwrap().unwrap();
        assert_eq!(saved.len(), 1);
        assert!(image.exists());

        // 戻れば何も起きず、見送った回数も数え直す。
        misskey.set_emojis(vec![emoji("a", None, &[], "u-a")]);
        job.run().await.unwrap();
        misskey.set_emojis(Vec::new());
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
        assert_eq!(shared.read().unwrap().as_ref().unwrap().len(), 1);

        // 2 回続けば、全部の削除として受け入れる。
        job.run().await.unwrap();
        let texts = misskey.texts();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].starts_with("【絵文字削除】"), "{}", texts[0]);
        assert!(shared.read().unwrap().as_ref().unwrap().is_empty());
        assert!(!image.exists());
    }

    #[tokio::test]
    async fn a_run_without_a_visual_change_does_not_upload() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", Some("c"), &[], "u-a")]);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.banner]\n"),
            shared(),
        );
        job.run().await.unwrap();
        job.run().await.unwrap();
        misskey.set_emojis(vec![emoji("a", Some("d"), &[], "u-a")]);
        job.run().await.unwrap();
        assert_eq!(misskey.uploads(), 1);
        assert!(
            misskey.texts().is_empty(),
            "no notify section, so nothing is posted"
        );
    }

    #[tokio::test]
    async fn enabling_the_banner_later_makes_it() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let shared = shared();
        EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared.clone(),
        )
        .run()
        .await
        .unwrap();
        assert_eq!(misskey.uploads(), 0);
        let config = config(&dir, "[emoji.notify]\n[emoji.banner]\n");
        EmojiCheckJob::new(&misskey, fetcher(), &config, shared.clone())
            .run()
            .await
            .unwrap();
        assert!(misskey.current_banner().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_banner_removed_or_replaced_by_someone_is_made_again() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.banner]\n"),
            shared(),
        );
        job.run().await.unwrap();
        let banner = misskey.current_banner().await.unwrap().unwrap();

        misskey.delete_file(&banner.id).await.unwrap();
        job.run().await.unwrap();
        assert_eq!(misskey.uploads(), 2);
        assert_eq!(
            misskey.current_banner().await.unwrap().unwrap().name,
            banner.name
        );

        let photo = misskey
            .upload_file("photo.jpg", "image/jpeg", vec![1])
            .await
            .unwrap();
        misskey.set_banner(Some(&photo)).await.unwrap();
        job.run().await.unwrap();
        assert_eq!(
            misskey.current_banner().await.unwrap().unwrap().name,
            banner.name
        );
        assert!(misskey.file_names().contains(&"photo.jpg".to_owned()));
    }

    #[tokio::test]
    async fn only_a_rename_that_changes_the_order_makes_the_banner_again() {
        let dir = TempDir::new();
        let fetcher = || {
            FakeFetcher::default()
                .with("u-red", Response::Image(PNG.to_vec(), None))
                .with("u-blue", Response::Image(BLUE.to_vec(), None))
        };
        let misskey = FakeMisskey::new().with_emojis(vec![
            emoji("a", None, &[], "u-red"),
            emoji("b", None, &[], "u-blue"),
        ]);
        let shared = shared();
        let config = config(&dir, "[emoji.banner]\n");
        let run = || EmojiCheckJob::new(&misskey, fetcher(), &config, shared.clone());
        run().run().await.unwrap();
        // 並び（赤、青）が変わらない名前変更。
        misskey.set_emojis(vec![
            emoji("aa", None, &[], "u-red"),
            emoji("b", None, &[], "u-blue"),
        ]);
        run().run().await.unwrap();
        assert_eq!(misskey.uploads(), 1);
        // 並びが（青、赤）に変わる名前変更。
        misskey.set_emojis(vec![
            emoji("z", None, &[], "u-red"),
            emoji("b", None, &[], "u-blue"),
        ]);
        run().run().await.unwrap();
        assert_eq!(misskey.uploads(), 2);
    }

    #[tokio::test]
    async fn a_banner_not_uploaded_by_the_bot_is_kept() {
        // bot の接頭辞で始まっていても、bot の付ける形でなければ所有者のファイル。
        for name in ["photo.jpg", "banner_summer.png"] {
            let dir = TempDir::new();
            let misskey = FakeMisskey::new()
                .with_emojis(vec![emoji("a", None, &[], "u-a")])
                .with_banner("theirs", name);
            let job = EmojiCheckJob::new(
                &misskey,
                fetcher(),
                &config(&dir, "[emoji.banner]\n"),
                shared(),
            );
            job.run().await.unwrap();
            misskey.set_emojis(vec![
                emoji("a", None, &[], "u-a"),
                emoji("b", None, &[], "u-b"),
            ]);
            job.run().await.unwrap();
            assert_ne!(misskey.banner_id().as_deref(), Some("theirs"), "{name}");
            assert!(misskey.file_names().contains(&name.to_owned()), "{name}");
        }
    }

    #[tokio::test]
    async fn a_banner_that_cannot_be_set_is_deleted_and_tried_again() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new()
            .with_emojis(vec![emoji("a", None, &[], "u-a")])
            .with_banner("old", "banner_0123456789abcdef.png")
            .failing(Fail::SetBanner);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.banner]\n"),
            shared(),
        );
        job.run().await.unwrap();
        assert_eq!(misskey.uploads(), 1);
        assert_eq!(misskey.file_names(), ["banner_0123456789abcdef.png"]);
        assert_eq!(misskey.banner_id().as_deref(), Some("old"));

        misskey.set_failing(None);
        job.run().await.unwrap();
        assert_eq!(misskey.uploads(), 2);
        let banner = misskey.current_banner().await.unwrap().unwrap();
        assert_ne!(banner.id, "old");
        assert_eq!(misskey.file_names(), [banner.name]);
    }

    #[tokio::test]
    async fn the_bot_banner_is_removed_when_all_emojis_are_deleted() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n[emoji.banner]\n"),
            shared(),
        );
        job.run().await.unwrap();
        assert!(misskey.banner_id().is_some());

        // 空の一覧が 2 回続いて全部の削除として受け入れたら、bot のバナーを外して消す。
        misskey.set_emojis(Vec::new());
        job.run().await.unwrap();
        job.run().await.unwrap();
        assert_eq!(misskey.banner_id(), None);
        assert!(misskey.file_names().is_empty());
        // 外したあとは何もしない。
        job.run().await.unwrap();
        assert_eq!(misskey.banner_id(), None);
        assert_eq!(misskey.uploads(), 1);
    }

    #[tokio::test]
    async fn a_banner_that_cannot_be_built_is_not_tried_again_until_the_list_changes() {
        let dir = TempDir::new();
        // デコードできない GIF。ETag があるので、2 回目は 304 になり、画像ファイルを書き直さない。
        let fetcher = FakeFetcher::default()
            .with("u-a", Response::Image(PNG.to_vec(), None))
            .with("u-b", Response::Cached(b"GIF89a..".to_vec(), "e1"));
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("b", None, &[], "u-b")]);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher,
            &config(&dir, "[emoji.banner]\n"),
            shared(),
        );
        job.run().await.unwrap();
        assert_eq!(misskey.uploads(), 0);

        // 画像ファイルがデコードできるものに替わっても、一覧が同じなら作り直さない。
        let gif = images::sha1_hex(b"GIF89a..");
        std::fs::write(dir.join("images").join(format!("{gif}.gif")), PNG).unwrap();
        job.run().await.unwrap();
        assert_eq!(misskey.uploads(), 0);

        // 一覧が変われば作り直す。
        misskey.set_emojis(vec![
            emoji("a", None, &[], "u-a"),
            emoji("b", None, &[], "u-b"),
        ]);
        job.run().await.unwrap();
        assert_eq!(misskey.uploads(), 1);
    }

    #[tokio::test]
    async fn nothing_is_posted_until_the_state_is_saved() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let shared = shared();
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared.clone(),
        );
        job.run().await.unwrap();
        misskey.set_emojis(vec![
            emoji("a", None, &[], "u-a"),
            emoji("b", None, &[], "u-b"),
        ]);
        // 一時ファイルの場所にディレクトリを置いて、保存を失敗させる。
        let temp = dir.join("emojis.json.tmp");
        std::fs::create_dir(&temp).unwrap();
        assert!(job.run().await.is_err());
        assert!(job.run().await.is_err());
        assert!(misskey.texts().is_empty());
        assert_eq!(shared.read().unwrap().as_ref().unwrap().len(), 1);

        std::fs::remove_dir(&temp).unwrap();
        job.run().await.unwrap();
        job.run().await.unwrap();
        assert_eq!(misskey.texts().len(), 1, "{:?}", misskey.texts());
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_stopped_after_saving_does_not_post_twice() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared(),
        );
        job.run().await.unwrap();
        misskey.set_emojis(vec![
            emoji("a", None, &[], "u-a"),
            emoji("b", None, &[], "u-b"),
        ]);
        // scheduler と同じく timeout で包み、投稿の途中で打ち切る。
        misskey.set_hanging(Some(Fail::Create));
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(60), job.run()).await;
        assert!(stopped.is_err());
        misskey.set_hanging(None);
        job.run().await.unwrap();
        assert!(
            misskey.texts().is_empty(),
            "the change was saved before posting, so it is not posted again"
        );
    }

    #[tokio::test]
    async fn many_additions_are_posted_in_several_notes() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared(),
        );
        job.run().await.unwrap();
        let mut emojis = vec![emoji("a", None, &[], "u-a")];
        let url = format!("https://misskey.example.com/files/{}", "0".repeat(36));
        emojis.extend((0..25).map(|i| emoji(&format!("bulk_{i:02}"), Some("bulk"), &[], &url)));
        misskey.set_emojis(emojis);
        job.run().await.unwrap();
        // fake は上限を超える本文を拒むので、全部が投稿されたなら分けられている。
        let texts = misskey.texts();
        assert!(texts.len() >= 2, "{texts:?}");
        assert!(texts.iter().all(|t| t.starts_with("【絵文字追加】\n")));
        for i in 0..25 {
            let name = format!("`:bulk_{i:02}:`");
            assert!(texts.iter().any(|t| t.contains(&name)), "{name}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn list_changes_are_posted_while_an_image_host_hangs() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().with_emojis(vec![emoji("a", None, &[], "u-a")]);
        let shared = shared();
        let config = config(&dir, "[emoji.notify]\n");
        EmojiCheckJob::new(&misskey, fetcher(), &config, shared.clone())
            .run()
            .await
            .unwrap();
        let before = shared
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .get("a")
            .unwrap()
            .image
            .clone();
        assert!(before.is_some());

        misskey.set_emojis(vec![
            emoji("a", Some("c"), &[], "u-a"),
            emoji("b", None, &[], "u-b"),
        ]);
        let hanging = FakeFetcher::default()
            .with("u-a", Response::Hang)
            .with("u-b", Response::Hang);
        // scheduler の timeout（30 分）より前に、期限で画像の取得を打ち切る。
        tokio::time::timeout(
            std::time::Duration::from_secs(30 * 60),
            EmojiCheckJob::new(&misskey, hanging, &config, shared.clone()).run(),
        )
        .await
        .expect("the run finishes before the job timeout")
        .unwrap();
        let texts = misskey.texts();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts[0].starts_with("【絵文字追加】\n$[x2 :b:]"));
        assert!(texts[1].starts_with("【カテゴリ変更】"));
        assert_eq!(
            shared
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .get("a")
                .unwrap()
                .image,
            before
        );
    }

    #[tokio::test]
    async fn a_failed_emoji_list_is_an_error() {
        let dir = TempDir::new();
        let misskey = FakeMisskey::new().failing(Fail::Emojis);
        let job = EmojiCheckJob::new(
            &misskey,
            fetcher(),
            &config(&dir, "[emoji.notify]\n"),
            shared(),
        );
        assert!(job.run().await.is_err());
    }

    #[test]
    fn only_names_made_by_banner_name_are_the_bots() {
        let set = EmojiSet::from_api(vec![emoji("a", None, &[], "u-a")]);
        assert!(is_bot_banner(&banner_name(&set)));
        for name in [
            "banner_summer.png",
            "banner_1.png",
            "banner_0123456789abcdef.jpg",
            "banner_0123456789ABCDEF.png",
            "banner_0123456789abcde.png",
        ] {
            assert!(!is_bot_banner(name), "{name}");
        }
    }
}
