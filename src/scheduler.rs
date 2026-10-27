//! cron 式の時刻にジョブを実行するループ。
//!
//! 起動直後に 1 回実行し、以降は次回時刻まで待って実行する。逐次に動くので重複実行は起きない。

use std::time::Duration;

use chrono::{DateTime, FixedOffset};
use croner::Cron;
use croner::errors::CronError;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info_span};

use crate::jobs::Job;

/// `shutdown` が立つまで、`cron` の時刻（JST）に `job` を実行し続ける。1 回の実行は `run_timeout` で打ち切り、次回に回す。
///
/// 実行中に停止を求められたら、その実行が終わってから戻る。途中で止めると、状態の保存のあとの投稿のような
/// 処理が抜けるため。`run_timeout` で打ち切ると途中で止まるので、ジョブは途中で止まっても重複投稿しない順に処理する。
pub async fn run_on_schedule<J: Job>(
    job: &J,
    cron: &Cron,
    run_timeout: Duration,
    shutdown: &CancellationToken,
) {
    run_once(job, run_timeout).await;
    loop {
        let now = crate::time::now();
        let next = match next_run(cron, now) {
            Ok(next) => next,
            Err(error) => {
                error!(job = job.name(), error = %error, "no next run time; this job will not run again");
                return;
            }
        };
        let wait = (next - now).to_std().unwrap_or_default();
        debug!(job = job.name(), next = %next.to_rfc3339(), "waiting for the next run");
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            () = sleep(wait) => {}
        }
        // sleep は単調時計で測るので、待つ間に壁時計が戻ると `next` より前に起きる。ここで実行すると、
        // 次の回に同じ `next` を選んでもう一度実行しうるので、待ち直す。
        if crate::time::now() < next {
            continue;
        }
        run_once(job, run_timeout).await;
    }
}

/// `now` より後で、最初に `cron` に合う時刻。
pub fn next_run(
    cron: &Cron,
    now: DateTime<FixedOffset>,
) -> Result<DateTime<FixedOffset>, CronError> {
    cron.find_next_occurrence(&now, false)
}

async fn run_once<J: Job>(job: &J, run_timeout: Duration) {
    let span = info_span!("job", name = job.name());
    async {
        match timeout(run_timeout, job.run()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => error!(error = %crate::error_chain(&error), "job failed"),
            Err(_) => error!(timeout = ?run_timeout, "job timed out"),
        }
    }
    .instrument(span)
    .await;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::Utc;

    use super::*;
    use crate::config::parse_cron;

    /// 実行した時刻を残し、`stop_after` 回目で停止を求めるジョブ。
    struct RecordingJob {
        ran_at: Mutex<Vec<DateTime<Utc>>>,
        stop_after: usize,
        shutdown: CancellationToken,
    }

    impl Job for RecordingJob {
        fn name(&self) -> &'static str {
            "recording"
        }

        async fn run(&self) -> Result<(), crate::jobs::Error> {
            let mut ran_at = self.ran_at.lock().unwrap();
            ran_at.push(Utc::now());
            if ran_at.len() >= self.stop_after {
                self.shutdown.cancel();
            }
            Ok(())
        }
    }

    fn at(value: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(value).unwrap()
    }

    #[test]
    fn next_run_is_strictly_after_now_in_the_given_timezone() {
        let every_ten_minutes = parse_cron("0 */10 * * * *").unwrap();
        assert_eq!(
            next_run(&every_ten_minutes, at("2026-10-14T09:00:00+09:00")).unwrap(),
            at("2026-10-14T09:10:00+09:00")
        );
        assert_eq!(
            next_run(&every_ten_minutes, at("2026-10-14T09:03:21+09:00")).unwrap(),
            at("2026-10-14T09:10:00+09:00")
        );
        let daily = parse_cron("0 0 0 * * *").unwrap();
        assert_eq!(
            next_run(&daily, at("2026-10-14T23:59:30+09:00")).unwrap(),
            at("2026-10-15T00:00:00+09:00")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn runs_once_at_startup_and_stops_when_asked() {
        let shutdown = CancellationToken::new();
        let job = RecordingJob {
            ran_at: Mutex::default(),
            stop_after: 1,
            shutdown: shutdown.clone(),
        };
        let yearly = parse_cron("0 0 0 1 1 *").unwrap();

        run_on_schedule(&job, &yearly, Duration::from_secs(60), &shutdown).await;

        assert_eq!(job.ran_at.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn runs_again_at_each_tick() {
        // 時計を止めると Utc::now が進まず、次回時刻が進むことを確かめられないので、実時間で毎秒の式を使う。
        let shutdown = CancellationToken::new();
        let job = RecordingJob {
            ran_at: Mutex::default(),
            stop_after: 3,
            shutdown: shutdown.clone(),
        };
        let every_second = parse_cron("* * * * * *").unwrap();

        timeout(
            Duration::from_secs(10),
            run_on_schedule(&job, &every_second, Duration::from_secs(60), &shutdown),
        )
        .await
        .unwrap();

        // 起動直後の 1 回の後は、毎秒の時刻ごとに 1 回ずつ実行する。
        let seconds: Vec<i64> = job
            .ran_at
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.timestamp())
            .collect();
        assert_eq!(seconds.len(), 3);
        assert!(seconds[1] < seconds[2], "{seconds:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn waits_again_when_woken_before_the_wall_clock_time() {
        // 止めた tokio の時計だけが進み、壁時計は進まない。待つ間に壁時計が戻ったのと同じになる。
        let shutdown = CancellationToken::new();
        let job = RecordingJob {
            ran_at: Mutex::default(),
            stop_after: 2,
            shutdown: shutdown.clone(),
        };
        let yearly = parse_cron("0 0 0 1 1 *").unwrap();

        let result = timeout(
            Duration::from_secs(400 * 24 * 60 * 60),
            run_on_schedule(&job, &yearly, Duration::from_secs(60), &shutdown),
        )
        .await;

        assert!(result.is_err(), "it keeps waiting for the wall clock");
        assert_eq!(job.ran_at.lock().unwrap().len(), 1);
    }
}
