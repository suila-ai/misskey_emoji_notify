//! ジョブ。Misskey から取ったものを Misskey に反映する「判断」の層。
//!
//! 外部 I/O は [`Misskey`](crate::misskey::Misskey) と [`ImageFetcher`](crate::images::ImageFetcher) の
//! トレイト越しに行い、テストではフェイクに差し替える。

pub mod emoji_check;

pub use emoji_check::EmojiCheckJob;

/// scheduler が定期に実行する 1 つの仕事。
pub trait Job {
    /// ログに出す名前。
    fn name(&self) -> &'static str;
    async fn run(&self) -> Result<(), Error>;
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
