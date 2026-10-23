//! Misskey のカスタム絵文字の変更を通知する bot。
//!
//! 絵文字一覧を定期に確かめて追加・削除・変更を投稿し、全絵文字を並べたバナーを作り、前日のリアクションのランキングを投稿する。

mod banner;
mod config;
mod emoji;
mod images;
mod jobs;
mod mfm;
mod misskey;
mod scheduler;
mod state;
mod time;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use croner::Cron;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::emoji::EmojiSet;
use crate::images::HttpImageFetcher;
use crate::jobs::{EmojiCheckJob, Job, RankingJob};
use crate::misskey::MiapiMisskey;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(error = %error_chain(&error), "startup failed");
            ExitCode::FAILURE
        }
    }
}

/// Misskey と画像の取得元を叩くときのタイムアウト。
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// 絵文字の確認とランキングの 1 回の実行に許す時間。初回は全絵文字の画像を取り、ランキングは 1 日分のノートを 100 件ずつ遡るので長い。
/// compose.yaml の `stop_grace_period` はこれより長くする。
const JOB_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 絵文字の一覧。起動時に控えから読み、絵文字の確認が差し替える。
/// `None` は控えが無いこと（初回）を表し、最初の確認で `Some` になる。
pub type SharedEmojiSet = Arc<RwLock<Option<Arc<EmojiSet>>>>;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("failed to load config")]
    Config(#[from] config::Error),
    #[error("failed to build the HTTP client")]
    Http(#[from] reqwest::Error),
    #[error("failed to connect to Misskey")]
    Misskey(#[from] misskey::Error),
    #[error(transparent)]
    State(#[from] state::Error),
}

/// 設定を読み、Misskey につなぎ、停止を求められるまでジョブを回す。
async fn run() -> Result<(), Error> {
    let path = config_path();
    let config = config::Config::load(&path)?;
    info!(path = %path.display(), server = %config.misskey.server, "config loaded");

    let http = build_http_client()?;
    let misskey =
        MiapiMisskey::connect(&config.misskey.server, &config.misskey.token, http.clone()).await?;

    let emoji_set = SharedEmojiSet::default();

    let shutdown = CancellationToken::new();
    let emoji_check = if config.needs_emoji_check() {
        // 状態の置き場は絵文字一覧の確認にしか使わない。確認しない設定でも作ると、作れない場所では起動に失敗する。
        state::create_dir(&config.emoji.state_dir)?;
        *emoji_set
            .write()
            .expect("the emoji set lock is not poisoned") = load_emoji_set(&config).map(Arc::new);
        Some(EmojiCheckJob::new(
            &misskey,
            HttpImageFetcher::new(http),
            &config.emoji,
            emoji_set.clone(),
        ))
    } else {
        info!("no notify or banner is configured; the emoji check will not run");
        None
    };
    let ranking = config.ranking.as_ref().map(|ranking| {
        let job = RankingJob::new(&misskey, ranking, time::now, shutdown.clone());
        (job, &ranking.cron)
    });

    let signals = {
        let shutdown = shutdown.clone();
        async move {
            match shutdown_signal().await {
                Ok(()) => info!("shutdown requested"),
                Err(error) => error!(error = %error, "failed to listen for signals; shutting down"),
            }
            shutdown.cancel();
        }
    };
    tokio::join!(
        signals,
        run_if_configured(emoji_check.map(|job| (job, &config.emoji.cron)), &shutdown),
        run_if_configured(ranking, &shutdown),
    );
    info!("stopped");
    Ok(())
}

/// 設定のあるジョブだけを、その cron 式の時刻に回す。回すジョブは起動時に名前を出し、設定が無くて回らないジョブをログで見分けられるようにする。
async fn run_if_configured<J: Job>(job: Option<(J, &Cron)>, shutdown: &CancellationToken) {
    if let Some((job, cron)) = job {
        info!(job = job.name(), "scheduled");
        scheduler::run_on_schedule(&job, cron, JOB_TIMEOUT, shutdown).await;
    }
}

/// 前回の絵文字一覧を状態ファイルから読む。無いか読めなければ `None`（初回として、次の確認では投稿しない）。
fn load_emoji_set(config: &config::Config) -> Option<EmojiSet> {
    let path = emoji::state_file(&config.emoji.state_dir);
    match state::load::<Vec<emoji::StoredEmoji>>(&path) {
        Ok(Some(emojis)) => {
            let set = EmojiSet::from_stored(emojis);
            info!(path = %path.display(), emojis = set.len(), "emoji state loaded");
            Some(set)
        }
        Ok(None) => {
            info!(path = %path.display(), "no emoji state; the first check will only take a snapshot");
            None
        }
        Err(error) => {
            warn!(path = %path.display(), error = %error_chain(&error), "starting without the emoji state; the first check will only take a snapshot");
            None
        }
    }
}

/// 設定ファイルのパス。第 1 引数で、無ければ作業ディレクトリの `config.toml`。
fn config_path() -> PathBuf {
    std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("config.toml"), PathBuf::from)
}

/// `RUST_LOG` があればそれに従い、無ければ info 以上を出す。
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Misskey と画像の取得元を叩く HTTP クライアント。UA とタイムアウトを揃え、`Clone` して渡す。
///
/// miapi のストリーミング（WebSocket の upgrade）は HTTP/1.1 でしか成立しないので、HTTP/1.1 だけを使う。
fn build_http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(HTTP_TIMEOUT)
        .http1_only()
        .build()
}

/// SIGINT（Ctrl-C）か SIGTERM（`docker stop`）を待つ。
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

/// エラーとその原因を `: ` でつないで 1 行にする。
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
