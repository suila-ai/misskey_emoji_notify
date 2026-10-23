//! 前日のローカルタイムラインのリアクションを集計して投稿するジョブ。
//!
//! 投稿済みかは、今日（JST）の自分のノートを返信も含めて全部遡り、その日のヘッダがあるかで判定し、状態ファイルは持たない。
//! 対象は今日（JST）の前日だけで、連日止まっていた分は諦める。ノートが 0 件の日と、数えるリアクションが無い日は投稿しない。
//! 起動直後の実行は、今日の cron の時刻を過ぎていれば取りこぼした分として投稿し、まだならその時刻を待つ。
//! 一時的な失敗（通信、5xx、レートリミット）は run 全体を再試行する。最初に投稿済みかを確かめるので、
//! 投稿が届いていたのに失敗とみなした場合も二重には投稿しない。

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeDelta};
use croner::Cron;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{Clock, Error, Job};
use crate::config::{RankingConfig, Visibility};
use crate::mfm::ranking::{self, aggregate};
use crate::misskey::{Misskey, NewNote, Note, Until};

/// 1 ページの件数（`notes/local-timeline` と `users/notes` の上限）。
const PAGE_SIZE: u8 = 100;
/// run 全体を再試行するまでの待ち。1 分以上待つので、HTTP のタイムアウト（30 秒）のあとに届いたノートも
/// 投稿済みとして見つかる。タイムラインのページの取得も、ページごとには再試行せずここでやり直す。
/// ページごとにも再試行すると、待ちの合計がジョブの timeout（30 分）を超え、後ろの再試行が打ち切られるため。
/// 待っている間に停止を求められたら、やめて最後の失敗を返す。投稿の途中ではないので重複しない。
const RUN_RETRY_WAITS: [Duration; 4] = [
    Duration::from_secs(60),
    Duration::from_secs(2 * 60),
    Duration::from_secs(4 * 60),
    Duration::from_secs(8 * 60),
];

pub struct RankingJob<'a, M> {
    misskey: &'a M,
    visibility: Visibility,
    cron: Cron,
    clock: Clock,
    shutdown: CancellationToken,
}

impl<'a, M: Misskey> RankingJob<'a, M> {
    pub fn new(
        misskey: &'a M,
        config: &RankingConfig,
        clock: Clock,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            misskey,
            visibility: config.visibility,
            cron: config.cron.clone(),
            clock,
            shutdown,
        }
    }

    /// 今日の cron の時刻を過ぎたか。起動直後の実行は、過ぎていれば取りこぼした分として投稿し、まだならその時刻を待つ。
    /// 0 時の cron（既定）を今日の時刻として拾えるよう、0 時ちょうども含めて探す。
    fn is_due(&self, now: DateTime<FixedOffset>) -> bool {
        self.cron
            .find_next_occurrence(&day_start(now.date_naive(), *now.offset()), true)
            .is_ok_and(|first| first <= now)
    }
}

/// `date` の 0 時。
fn day_start(date: NaiveDate, offset: FixedOffset) -> DateTime<FixedOffset> {
    date.and_time(NaiveTime::MIN)
        .and_local_timezone(offset)
        .single()
        .expect("a fixed offset has exactly one midnight")
}

/// `date`（JST）のノートを、新しいものから 1 ページずつ遡って `visit` に渡す。ページは `fetch` が `Until` より前を新しい順に返す。
/// `visit` が `Break` を返したら、そこで止める。
async fn scan_day<F, Fut>(
    date: NaiveDate,
    offset: FixedOffset,
    mut fetch: F,
    mut visit: impl FnMut(Note) -> ControlFlow<()>,
) -> Result<(), crate::misskey::Error>
where
    F: FnMut(Until) -> Fut,
    Fut: Future<Output = Result<Vec<Note>, crate::misskey::Error>>,
{
    let since = day_start(date, offset).to_utc();
    let mut until = Until::Date(since + TimeDelta::days(1));
    loop {
        let fetched = fetch(until.clone()).await?;
        let Some(last) = fetched.last() else {
            return Ok(());
        };
        let reached_start = last.created_at < since;
        let next = Until::Id(last.id.clone());
        for note in fetched.into_iter().filter(|note| note.created_at >= since) {
            if visit(note).is_break() {
                return Ok(());
            }
        }
        if reached_start || next == until {
            return Ok(());
        }
        until = next;
    }
}

impl<M: Misskey> Job for RankingJob<'_, M> {
    fn name(&self) -> &'static str {
        "ranking"
    }

    async fn run(&self) -> Result<(), Error> {
        let now = (self.clock)();
        if !self.is_due(now) {
            debug!(now = %now.to_rfc3339(), "today's posting time has not come yet");
            return Ok(());
        }
        let mut waits = RUN_RETRY_WAITS.into_iter();
        loop {
            let error = match self.run_once().await {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };
            let transient =
                matches!(&error, Error::Misskey(error) if crate::misskey::is_transient(error));
            match waits.next() {
                Some(wait) if transient => {
                    warn!(wait = ?wait, error = %crate::error_chain(&error), "failed to post the ranking; retrying");
                    tokio::select! {
                        biased;
                        () = self.shutdown.cancelled() => return Err(error),
                        () = tokio::time::sleep(wait) => {}
                    }
                }
                _ => return Err(error),
            }
        }
    }
}

impl<M: Misskey> RankingJob<'_, M> {
    /// 投稿済みかを確かめ、まだなら前日の分を集計して投稿する。
    async fn run_once(&self) -> Result<(), Error> {
        let now = (self.clock)();
        let (today, offset) = (now.date_naive(), *now.offset());
        let target = today.pred_opt().expect("the day before today exists");
        let mut posted = None;
        scan_day(
            today,
            offset,
            |until| async move { self.misskey.my_notes(&until, PAGE_SIZE).await },
            |note| {
                if ranking::is_for(&note.text, target) {
                    posted = Some(note.id);
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )
        .await?;
        if let Some(note_id) = posted {
            debug!(%note_id, %target, "already posted");
            return Ok(());
        }

        let mut notes = 0;
        let mut reactions: HashMap<String, u64> = HashMap::new();
        scan_day(
            target,
            offset,
            |until| async move { self.misskey.local_timeline(&until, PAGE_SIZE).await },
            |note| {
                notes += 1;
                for (key, count) in note.reactions {
                    *reactions.entry(key).or_default() += count;
                }
                ControlFlow::Continue(())
            },
        )
        .await?;
        if notes == 0 {
            info!(%target, "no note on the local timeline; nothing is posted");
            return Ok(());
        }
        let ranking = aggregate(
            target,
            notes,
            reactions.iter().map(|(key, count)| (key.as_str(), *count)),
        );
        if ranking.groups.is_empty() {
            info!(%target, notes = ranking.notes, "no local or Unicode reaction; nothing is posted");
            return Ok(());
        }
        let text = ranking::render(&ranking);
        let note = self
            .misskey
            .create_note(NewNote {
                text: &text,
                visibility: self.visibility,
            })
            .await?;
        info!(note_id = %note.id, %target, notes = ranking.notes, reactions = ranking.reactions, "posted");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::misskey::fake::{Fail, FakeMisskey, note};
    use crate::time::JST;
    use chrono::Utc;

    /// 2026-10-23 00:05 JST。
    fn clock() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-10-23T00:05:00+09:00").unwrap()
    }

    fn config() -> RankingConfig {
        config_at("0 0 0 * * *")
    }

    fn config_at(cron: &str) -> RankingConfig {
        RankingConfig {
            visibility: Visibility::Home,
            cron: crate::config::parse_cron(cron).unwrap(),
        }
    }

    /// 2026-10-23 05:00 JST。
    fn five_am() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-10-23T05:00:00+09:00").unwrap()
    }

    /// 2026-10-23 13:00 JST。
    fn one_pm() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-10-23T13:00:00+09:00").unwrap()
    }

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value).unwrap().to_utc()
    }

    fn reacted(id: &str, created_at: &str, reactions: &[(&str, u64)]) -> Note {
        let mut n = note(id, "", at(created_at));
        n.reactions = reactions
            .iter()
            .map(|(k, v)| ((*k).to_owned(), *v))
            .collect::<HashMap<_, _>>();
        n
    }

    #[tokio::test]
    async fn posts_yesterdays_ranking_once() {
        let misskey = FakeMisskey::new()
            .with_base_time(clock().to_utc())
            .with_timeline(vec![
                reacted(
                    "n1",
                    "2026-10-22T10:00:00+09:00",
                    &[(":a@.:", 2), ("👍", 1)],
                ),
                reacted(
                    "n2",
                    "2026-10-22T23:59:59+09:00",
                    &[(":a@.:", 1), (":r@example.com:", 5)],
                ),
                // 対象日の外。
                reacted("n3", "2026-10-23T00:00:00+09:00", &[(":z@.:", 9)]),
                reacted("n0", "2026-10-21T23:59:59+09:00", &[(":y@.:", 9)]),
            ]);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        job.run().await.unwrap();
        assert_eq!(
            misskey.texts(),
            [
                "【昨日(10/22)のリアクション】\n$[x2 🥇 :a:] x3\n🥈 👍 x1\n\nノート数: 2 リアクション数: 4"
            ]
        );
        // 2 回目は投稿しない。
        job.run().await.unwrap();
        assert_eq!(misskey.texts().len(), 1);
        // 最初のページは対象日の翌日 0 時（JST）から。
        assert_eq!(
            misskey.timeline_queries()[0],
            Until::Date(at("2026-10-23T00:00:00+09:00").to_utc())
        );
    }

    #[tokio::test]
    async fn already_posted_is_detected_from_todays_notes() {
        let misskey = FakeMisskey::new()
            .with_base_time(clock().to_utc())
            .with_own_note(
                "【昨日(10/22)のリアクション】\n$[x2 🥇 :a:] x3\n\nノート数: 1 リアクション数: 3",
            )
            .with_timeline(vec![reacted(
                "n1",
                "2026-10-22T10:00:00+09:00",
                &[(":a@.:", 2)],
            )]);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        job.run().await.unwrap();
        assert_eq!(misskey.texts().len(), 1);
        assert!(
            misskey.timeline_queries().is_empty(),
            "the timeline is not fetched"
        );
    }

    #[tokio::test]
    async fn a_ranking_behind_more_than_a_page_of_own_notes_is_not_posted_again() {
        // ランキングのあとにメンションへの返信が 1 ページより多く続いても、遡って見つける。
        let misskey = (0..150)
            .fold(
                FakeMisskey::new().with_base_time(clock().to_utc()).with_own_note(
                    "【昨日(10/22)のリアクション】\n$[x2 🥇 :a:] x3\n\nノート数: 1 リアクション数: 3",
                ),
                |misskey, i| misskey.with_own_note(&format!("返信 {i}")),
            )
            .with_timeline(vec![reacted(
                "n1",
                "2026-10-22T10:00:00+09:00",
                &[(":a@.:", 2)],
            )]);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        job.run().await.unwrap();
        assert_eq!(misskey.texts().len(), 151);
        assert!(misskey.timeline_queries().is_empty());
    }

    #[tokio::test]
    async fn pages_through_the_day_and_stops_at_its_start() {
        let notes: Vec<Note> = (0..250)
            .map(|i| {
                let t = day_start(NaiveDate::from_ymd_opt(2026, 10, 22).unwrap(), JST).to_utc()
                    + TimeDelta::minutes(i);
                reacted(&format!("n{i}"), &t.to_rfc3339(), &[(":a@.:", 1)])
            })
            .chain([reacted(
                "old",
                "2026-10-21T12:00:00+09:00",
                &[(":a@.:", 100)],
            )])
            .collect();
        let misskey = FakeMisskey::new().with_timeline(notes);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        job.run().await.unwrap();
        let text = misskey.texts().pop().unwrap();
        assert!(text.contains("ノート数: 250 リアクション数: 250"), "{text}");
        // 100 件ずつ遡り、3 ページ目（対象日の残り 50 件と前日の 1 件）で前日のノートに達して止まる。
        assert_eq!(misskey.timeline_queries().len(), 3);
    }

    #[tokio::test]
    async fn an_empty_day_posts_nothing() {
        let misskey = FakeMisskey::new();
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
    }

    #[tokio::test]
    async fn a_day_without_countable_reactions_posts_nothing() {
        let misskey = FakeMisskey::new()
            .with_base_time(clock().to_utc())
            .with_timeline(vec![
                reacted("n1", "2026-10-22T10:00:00+09:00", &[(":r@example.com:", 5)]),
                reacted("n2", "2026-10-22T11:00:00+09:00", &[]),
            ]);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_timeline_is_retried_with_backoff_and_then_fails() {
        let misskey = FakeMisskey::new().failing(Fail::Timeline);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        let started = tokio::time::Instant::now();
        assert!(job.run().await.is_err());
        // ページごとには再試行せず、run 全体を 1, 2, 4, 8 分待って 5 回試す。
        assert_eq!(started.elapsed(), Duration::from_secs((1 + 2 + 4 + 8) * 60));
        assert!(misskey.texts().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_shutdown_during_a_retry_wait_stops_the_run() {
        let misskey = FakeMisskey::new().failing(Fail::Timeline);
        let shutdown = CancellationToken::new();
        let job = RankingJob::new(&misskey, &config(), clock, shutdown.clone());
        let cancel = async {
            tokio::time::sleep(Duration::from_secs(90)).await;
            shutdown.cancel();
        };
        let started = tokio::time::Instant::now();
        let (result, ()) = tokio::join!(job.run(), cancel);
        // 1 分の待ちのあとの 2 回目も失敗し、2 分の待ちの途中で止まる。
        assert!(result.is_err());
        assert_eq!(started.elapsed(), Duration::from_secs(90));
    }

    fn yesterday_with_a_reaction() -> FakeMisskey {
        FakeMisskey::new()
            .with_base_time(clock().to_utc())
            .with_timeline(vec![reacted(
                "n1",
                "2026-10-22T10:00:00+09:00",
                &[("👍", 1)],
            )])
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_failure_is_retried_and_posts_once() {
        let misskey = yesterday_with_a_reaction().failing_times(Fail::Create, 2);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        let started = tokio::time::Instant::now();
        job.run().await.unwrap();
        assert_eq!(misskey.texts().len(), 1);
        assert_eq!(started.elapsed(), Duration::from_secs(60 + 2 * 60));
    }

    #[tokio::test(start_paused = true)]
    async fn a_post_stored_despite_an_error_is_not_posted_again() {
        let misskey = yesterday_with_a_reaction().failing_times(Fail::CreateStored, 1);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        job.run().await.unwrap();
        assert_eq!(misskey.texts().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_post_is_not_retried() {
        let misskey = yesterday_with_a_reaction().failing(Fail::CreateInvalid);
        let job = RankingJob::new(&misskey, &config(), clock, CancellationToken::new());
        let started = tokio::time::Instant::now();
        assert!(job.run().await.is_err());
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test]
    async fn nothing_is_done_before_todays_scheduled_time() {
        let misskey = yesterday_with_a_reaction();
        let job = RankingJob::new(
            &misskey,
            &config_at("0 0 12 * * *"),
            five_am,
            CancellationToken::new(),
        );
        job.run().await.unwrap();
        assert!(misskey.texts().is_empty());
        assert!(misskey.timeline_queries().is_empty());
    }

    #[tokio::test]
    async fn a_missed_time_earlier_today_is_caught_up() {
        let misskey = yesterday_with_a_reaction();
        let job = RankingJob::new(
            &misskey,
            &config_at("0 0 12 * * *"),
            one_pm,
            CancellationToken::new(),
        );
        job.run().await.unwrap();
        assert_eq!(misskey.texts().len(), 1);
    }
}
