//! ジョブ。Misskey から取ったものを Misskey に反映する「判断」の層。
//!
//! 外部 I/O は [`Misskey`](crate::misskey::Misskey) と [`ImageFetcher`](crate::images::ImageFetcher) の
//! トレイト越しに行い、テストではフェイクに差し替える。今の時刻も `clock` で差し替える。

pub mod emoji_check;
pub mod ranking;
pub mod unicode_emoji;

pub use emoji_check::EmojiCheckJob;
pub use ranking::RankingJob;
pub use unicode_emoji::UnicodeEmojiCheckJob;

use chrono::{DateTime, FixedOffset};
use tracing::{error, info};

use crate::config::Visibility;
use crate::misskey::{Misskey, NewNote};

/// scheduler が定期に実行する 1 つの仕事。
pub trait Job {
    /// ログに出す名前。
    fn name(&self) -> &'static str;
    async fn run(&self) -> Result<(), Error>;
}

/// 今の時刻（JST）。テストでは決まった時刻を返す関数に差し替える。
pub type Clock = fn() -> DateTime<FixedOffset>;

/// 今の span の中で、`f` を別スレッドで動かす。
pub(crate) fn spawn_blocking_in_span<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || span.in_scope(f))
}

/// `texts` を順にノートにする。失敗した本文は error ログを出して続け、呼び出し側は次回また投稿しない。
pub(crate) async fn post_texts<M: Misskey>(misskey: &M, visibility: Visibility, texts: &[String]) {
    for text in texts {
        match misskey
            .create_note(NewNote {
                text,
                visibility,
                reply_id: None,
            })
            .await
        {
            Ok(note) => info!(note_id = %note.id, "posted"),
            Err(error) => {
                error!(error = %crate::error_chain(&error), length = crate::mfm::note_length(text), "failed to post a change; it will not be retried")
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Misskey(#[from] crate::misskey::Error),
    #[error(transparent)]
    State(#[from] crate::state::Error),
    #[error("a blocking task panicked")]
    Task(#[from] tokio::task::JoinError),
}
