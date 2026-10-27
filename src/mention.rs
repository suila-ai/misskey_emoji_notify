//! メンションへの応答。ストリーミングで受けたメンションに、ガチャと占いで返信する。
//!
//! bot からのメンションは無視し、設定によってはフォローしていない相手も無視する。ハンドラをガチャ、占いの順に試し、
//! 返信できたら何もせず、途中で失敗したら ⚠️ を、どれにも当たらなければ ❓ をリアクションする。
//! ストリーミングにつながらなければ、つながるまで待ちを延ばしながら試し続ける。つながったら、つながるまでの間と
//! 切れていた間のメンションを REST で取り直して応答する。取り直しの一時的な失敗も、取れるまで試し直す。

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

use rand::seq::IndexedRandom;
use sha1::{Digest, Sha1};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::SharedEmojiSet;
use crate::config::{FortuneConfig, GachaConfig, MentionConfig, RequireFollowed, Visibility};
use crate::emoji::EmojiSet;
use crate::jobs::Clock;
use crate::mfm::mention::{self as text, Luck};
use crate::misskey::{Error, Event, MentionSource, Misskey, NewNote, Note, Since};
use crate::ssv;

/// ガチャで一度に引ける数の上限。
const MAX_GACHA: usize = 50;
/// ガチャの呼び出しの語。
const GACHA_WORDS: [&str; 2] = ["ガチャ", "gacha"];
/// 占いの呼び出しの語。本文か CW にどれかを含めば占う。
const FORTUNE_WORDS: [&str; 4] = ["占", "うらな", "運勢", "おみくじ"];
/// 途中で失敗したときのリアクション。
const REACTION_FAILED: &str = "⚠️";
/// どのハンドラにも当たらなかったときのリアクション。
const REACTION_UNKNOWN: &str = "❓";
/// ストリーミングへの接続と取り直しを試し直すまでの、最初の待ちと上限。失敗のたびに倍にする。
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);
/// 取り直すときの 1 ページの件数と、ページの数の上限。
const MISSED_PAGE: u8 = 100;
const MISSED_PAGES: usize = 10;
/// 取り直したメンションのうち、これより古いものには応答しない。今から返信しても遅いため。
const MISSED_MAX_AGE: TimeDelta = TimeDelta::minutes(30);
/// 応答したメンションの ID を覚えておく数。取り直しとストリームの両方で届いたものに 2 回応答しないため。
const ANSWERED_CAPACITY: usize = 1000;

/// メンション 1 件の扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// bot か、フォローしていない相手。
    Ignored,
    Replied,
    /// ハンドラが失敗し、⚠️ を付けた。
    Failed,
    /// どのハンドラにも当たらず、❓ を付けた。
    Unknown,
}

/// ハンドラの失敗。
#[derive(Debug, thiserror::Error)]
enum HandlerError {
    #[error("the emoji list is empty (not fetched yet?)")]
    NoEmojis,
    #[error("no emoji is in the fortune categories")]
    NoEmojisInCategories,
    #[error(transparent)]
    Misskey(#[from] Error),
}

pub struct Responder<'a, M> {
    misskey: &'a M,
    require_followed: RequireFollowed,
    gacha: Option<&'a GachaConfig>,
    fortune: Option<&'a FortuneConfig>,
    emoji_set: SharedEmojiSet,
    clock: Clock,
}

impl<'a, M: Misskey> Responder<'a, M> {
    pub fn new(
        misskey: &'a M,
        config: &'a MentionConfig,
        emoji_set: SharedEmojiSet,
        clock: Clock,
    ) -> Self {
        Self {
            misskey,
            require_followed: config.require_followed,
            gacha: config.gacha.as_ref(),
            fortune: config.fortune.as_ref(),
            emoji_set,
            clock,
        }
    }

    /// 今の絵文字一覧。まだ一度も確認していなければ空。
    fn emojis(&self) -> Arc<EmojiSet> {
        self.emoji_set
            .read()
            .expect("the emoji set lock is not poisoned")
            .clone()
            .unwrap_or_default()
    }

    /// メンション 1 件に応答する。
    pub async fn handle(&self, note: &Note) -> Result<Outcome, Error> {
        if note.user.is_bot {
            return Ok(Outcome::Ignored);
        }
        let must_follow = match self.require_followed {
            RequireFollowed::None => false,
            RequireFollowed::Remote => note.user.host.is_some(),
            RequireFollowed::All => true,
        };
        if must_follow && !self.misskey.is_followed_by(&note.user.id).await? {
            info!(note_id = %note.id, user_id = %note.user.id, "ignored a mention from a user who does not follow");
            return Ok(Outcome::Ignored);
        }

        let result = match self.gacha(note).await {
            Some(result) => Some(result),
            None => self.fortune(note).await,
        };
        match result {
            Some(Ok(())) => Ok(Outcome::Replied),
            Some(Err(error)) => {
                warn!(note_id = %note.id, error = %crate::error_chain(&error), "failed to answer a mention");
                self.misskey.react(&note.id, REACTION_FAILED).await?;
                Ok(Outcome::Failed)
            }
            None => {
                self.misskey.react(&note.id, REACTION_UNKNOWN).await?;
                Ok(Outcome::Unknown)
            }
        }
    }

    async fn reply(&self, note: &Note, text: &str, visibility: Visibility) -> Result<(), Error> {
        // specified のノートには specified でしか返信できない。
        let visibility = if note.is_specified {
            Visibility::Specified
        } else {
            visibility
        };
        let reply = self
            .misskey
            .create_note(NewNote {
                text,
                visibility,
                reply_id: Some(&note.id),
            })
            .await?;
        info!(note_id = %note.id, reply_id = %reply.id, "replied");
        Ok(())
    }

    /// ガチャ。`ガチャ [個数] [カテゴリ...]` の形で、本文に無ければ CW から読む。当たらなければ `None`。
    async fn gacha(&self, note: &Note) -> Option<Result<(), HandlerError>> {
        let config = self.gacha?;
        let fields = body_and_cw(note).find_map(|text| {
            let mut fields = ssv::parse(text);
            let start = fields
                .iter()
                .position(|field| GACHA_WORDS.contains(&field.as_str()))?;
            Some(fields.split_off(start))
        })?;
        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        let (count, categories): (usize, Vec<String>) = match fields.as_slice() {
            [_] => (1, Vec::new()),
            [_, count, categories @ ..] => {
                let count: usize = count.parse().ok().filter(|count| *count > 0)?;
                (
                    count.min(MAX_GACHA),
                    categories.iter().map(|c| (*c).to_owned()).collect(),
                )
            }
            [] => return None,
        };
        let emojis = self.emojis();
        if emojis.is_empty() {
            return Some(Err(HandlerError::NoEmojis));
        }
        let mut pool = emojis.in_categories(&categories);
        if pool.is_empty() {
            pool = emojis.iter().collect();
        }
        let names: Vec<&str> = {
            let mut rng = rand::rng();
            (0..count)
                .map(|_| {
                    pool.choose(&mut rng)
                        .expect("the pool is not empty")
                        .name
                        .as_str()
                })
                .collect()
        };
        Some(
            self.reply(note, &text::gacha(&names), config.visibility)
                .await
                .map_err(Into::into),
        )
    }

    /// 占い。本文か CW に占いの語があれば、その日とユーザーで決まる結果を返す。
    async fn fortune(&self, note: &Note) -> Option<Result<(), HandlerError>> {
        let config = self.fortune?;
        if !body_and_cw(note).any(|text| FORTUNE_WORDS.iter().any(|word| text.contains(word))) {
            return None;
        }
        let emojis = self.emojis();
        if emojis.is_empty() {
            return Some(Err(HandlerError::NoEmojis));
        }
        let candidates = emojis.in_categories(&config.categories);
        if candidates.is_empty() {
            return Some(Err(HandlerError::NoEmojisInCategories));
        }
        let all: Vec<_> = emojis.iter().collect();
        let date = (self.clock)().date_naive();
        let [first, second, third] = draws(&date.format("%Y%m%d").to_string(), &note.user.id);
        let emoji = &candidates[(first % candidates.len() as u64) as usize].name;
        let luck = if second % 3 < 2 {
            Luck::Good
        } else {
            Luck::Bad
        };
        let lucky = &all[(third % all.len() as u64) as usize].name;
        Some(
            self.reply(note, &text::fortune(emoji, luck, lucky), config.visibility)
                .await
                .map_err(Into::into),
        )
    }
}

/// 日付とユーザー ID で決まる 3 つの値。SHA-1 の 20 バイトを 8 バイトずつ（末尾は重ねて）読む。
/// 乱数を使わないので、プロセスをまたいでも同じ日の同じユーザーには同じ結果になる。
fn draws(date: &str, user_id: &str) -> [u64; 3] {
    let digest = Sha1::digest(format!("{date}{user_id}").as_bytes());
    let read =
        |offset: usize| u64::from_be_bytes(digest[offset..offset + 8].try_into().expect("8 bytes"));
    [read(0), read(8), read(12)]
}

/// 呼び出しを探す文。本文、CW の順。
fn body_and_cw(note: &Note) -> impl Iterator<Item = &str> {
    std::iter::once(note.text.as_str()).chain(note.cw.as_deref())
}

/// `open` でストリーミングにつなぐ。失敗したら待ちを延ばしながら、つながるか `shutdown` が立つまで試し続ける。
/// `shutdown` が立ったら `None`。
pub async fn connect<S, F>(open: impl FnMut() -> F, shutdown: &CancellationToken) -> Option<S>
where
    F: Future<Output = Result<S, Error>>,
{
    // `is_transient` で分けず、どの失敗でも試し直す。サーバに拒まれた（401 など）ときも、miapi のつなぎ直しと同じく試し続け、
    // プロキシなどの設定が直れば再起動せずにつながる。
    tokio::select! {
        biased;
        () = shutdown.cancelled() => None,
        result = retry("open the streaming connection", open, |_| true) => result.ok(),
    }
}

/// `attempt` が成功するまで、待ちを延ばしながら試し直す。`retryable` でない失敗はそのまま返す。
/// 停止は見ないので、呼び出し側が `shutdown` と `select!` して future ごと落とす。
async fn retry<T, F>(
    action: &str,
    mut attempt: impl FnMut() -> F,
    retryable: fn(&Error) -> bool,
) -> Result<T, Error>
where
    F: Future<Output = Result<T, Error>>,
{
    let mut wait = FIRST_RETRY_WAIT;
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) if retryable(&error) => {
                warn!(wait = ?wait, error = %crate::error_chain(&error), "failed to {action}; retrying")
            }
            Err(error) => return Err(error),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(MAX_RETRY_WAIT);
    }
}

/// 受けたメンションの ID。2 回応答しないためと、取り直しの起点に使う。
struct Answered {
    opened_at: DateTime<Utc>,
    /// 新しいほうから [`ANSWERED_CAPACITY`] 件。Misskey の ID は時刻の順に並ぶ文字列なので、最後が最新。
    ids: BTreeSet<String>,
}

impl Answered {
    fn new(opened_at: DateTime<Utc>) -> Self {
        Self {
            opened_at,
            ids: BTreeSet::new(),
        }
    }

    /// 初めて受けた ID なら控えて `true`。
    fn insert(&mut self, id: &str) -> bool {
        if !self.ids.insert(id.to_owned()) {
            return false;
        }
        if self.ids.len() > ANSWERED_CAPACITY {
            self.ids.pop_first();
        }
        true
    }

    /// 取り直しの起点。受けたうちで最大の ID。まだ受けていなければ、つなぎ始めた時刻。
    fn since(&self) -> Since {
        self.ids
            .last()
            .map_or(Since::Date(self.opened_at), |id| Since::Id(id.clone()))
    }
}

/// `shutdown` が立つか、ストリームが閉じるまでメンションに応答し続ける。`opened_at` はストリームにつなぎ始めた時刻で、
/// 最初の取り直しの起点にする。
pub async fn run<S: MentionSource, M: Misskey>(
    mut source: S,
    responder: &Responder<'_, M>,
    opened_at: DateTime<Utc>,
    shutdown: &CancellationToken,
) {
    let mut answered = Answered::new(opened_at);
    // つながるまで試し直していた間のメンションはストリームに届かないので、最初にも取り直す。
    tokio::select! {
        biased;
        () = shutdown.cancelled() => return,
        () = catch_up(responder, &mut answered) => {}
    }
    loop {
        let event = tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            event = source.recv() => event,
        };
        match event {
            None => {
                warn!("the mention stream closed; mentions will not be answered until restart");
                return;
            }
            Some(Err(error)) => {
                warn!(error = %crate::error_chain(&error), "could not read a mention")
            }
            Some(Ok(Event::Mention(note))) => {
                if answered.insert(&note.id) {
                    respond(responder, &note).await;
                }
            }
            Some(Ok(Event::Reconnected)) => {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    () = catch_up(responder, &mut answered) => {}
                }
            }
        }
    }
}

async fn respond<M: Misskey>(responder: &Responder<'_, M>, note: &Note) {
    if let Err(error) = responder.handle(note).await {
        warn!(note_id = %note.id, error = %crate::error_chain(&error), "failed to react to a mention");
    }
}

/// ストリームに届かなかったメンションを取り直し、古い順に応答する。応答済みのものと、古すぎるものは飛ばす。
///
/// 一時的な失敗は、取れるまで試し直す。試し直す間はストリームを読まないので起点が先へ進まず、切れていた間の
/// メンションを取りこぼさない（その間にストリームに届いたものは失われず、取り直したあとで重複として飛ばす）。
async fn catch_up<M: Misskey>(responder: &Responder<'_, M>, answered: &mut Answered) {
    let (mut replied, mut stale) = (0, 0);
    for _ in 0..MISSED_PAGES {
        let since = answered.since();
        let fetched = retry(
            "fetch the missed mentions",
            || responder.misskey.mentions_since(&since, MISSED_PAGE),
            crate::misskey::is_transient,
        )
        .await;
        let notes = match fetched {
            Ok(notes) => notes,
            Err(error) => {
                warn!(error = %crate::error_chain(&error), "failed to fetch the missed mentions; the rest is not answered");
                return;
            }
        };
        // 試し直しで時間が経っていても、古さは取ったときの時刻で決める。
        let now = (responder.clock)().to_utc();
        for note in &notes {
            if !answered.insert(&note.id) {
                continue;
            }
            if now - note.created_at > MISSED_MAX_AGE {
                stale += 1;
                continue;
            }
            replied += 1;
            respond(responder, note).await;
        }
        if notes.len() < usize::from(MISSED_PAGE) {
            break;
        }
    }
    info!(
        replied,
        skipped_as_old = stale,
        "caught up with the missed mentions"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::RwLock;

    use chrono::{DateTime, FixedOffset, Utc};

    use super::*;
    use crate::emoji::test_support::stored;
    use crate::misskey::fake::{Fail, FakeMentions, FakeMisskey, note};

    fn clock() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-10-23T12:00:00+09:00").unwrap()
    }

    fn emojis() -> SharedEmojiSet {
        Arc::new(RwLock::new(Some(Arc::new(EmojiSet::from_stored([
            stored("cat", Some("animal"), &[], "u1"),
            stored("dog", Some("animal"), &[], "u2"),
            stored("sun", Some("sky"), &[], "u3"),
        ])))))
    }

    fn config(require_followed: RequireFollowed, gacha: bool, fortune: bool) -> MentionConfig {
        MentionConfig {
            require_followed,
            gacha: gacha.then_some(GachaConfig {
                visibility: Visibility::Public,
            }),
            fortune: fortune.then(|| FortuneConfig {
                visibility: Visibility::Followers,
                categories: vec!["animal".to_owned()],
            }),
        }
    }

    fn mention(text: &str) -> Note {
        note("m1", text, Utc::now())
    }

    #[tokio::test]
    async fn gacha_replies_with_the_requested_number_of_emojis_from_the_categories() {
        let misskey = FakeMisskey::new();
        let cfg = config(RequireFollowed::None, true, true);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        assert_eq!(
            responder
                .handle(&mention("@bot ガチャ 3 sky"))
                .await
                .unwrap(),
            Outcome::Replied
        );
        let created = misskey.created();
        assert_eq!(created[0].note.text, ":sun::sun::sun:");
        assert_eq!(created[0].visibility, Visibility::Public);

        // 個数も指定も無ければ 1 つ。知らないカテゴリなら全絵文字から。上限は 50。
        responder.handle(&mention("gacha")).await.unwrap();
        responder.handle(&mention("ガチャ 100 nope")).await.unwrap();
        let texts = misskey.texts();
        assert_eq!(texts[1].matches(':').count(), 2);
        assert_eq!(texts[2].matches(':').count(), 100);
        assert!(misskey.reactions().is_empty());
    }

    #[tokio::test]
    async fn gacha_and_fortune_are_found_in_the_cw() {
        let misskey = FakeMisskey::new();
        let cfg = config(RequireFollowed::None, true, true);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let with_cw = |cw: &str, text: &str| Note {
            cw: Some(cw.to_owned()),
            ..mention(text)
        };
        for (cw, text) in [
            ("ガチャ 2", "@bot"),
            ("占って", "@bot"),
            ("見て", "@bot ガチャ 3"),
        ] {
            assert_eq!(
                responder.handle(&with_cw(cw, text)).await.unwrap(),
                Outcome::Replied,
                "{cw} / {text}"
            );
        }
        let texts = misskey.texts();
        // 本文にメンションしか無ければ、CW の呼び出しと引数を読む。
        assert_eq!(texts[0].matches(':').count(), 4);
        assert!(
            texts[1].starts_with("今日のあなたの運勢は\n"),
            "{}",
            texts[1]
        );
        // 本文に呼び出しがあれば、引数は本文から読む。
        assert_eq!(texts[2].matches(':').count(), 6);
    }

    #[tokio::test]
    async fn gacha_with_a_bad_count_is_unknown_and_an_empty_list_is_a_failure() {
        let misskey = FakeMisskey::new();
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        assert_eq!(
            responder.handle(&mention("ガチャ abc")).await.unwrap(),
            Outcome::Unknown
        );
        assert_eq!(
            responder.handle(&mention("ガチャ 0")).await.unwrap(),
            Outcome::Unknown
        );
        assert_eq!(
            responder.handle(&mention("こんにちは")).await.unwrap(),
            Outcome::Unknown
        );
        assert_eq!(misskey.reactions().len(), 3);
        assert!(misskey.reactions().iter().all(|(_, r)| r == "❓"));

        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, SharedEmojiSet::default(), clock);
        assert_eq!(
            responder.handle(&mention("ガチャ")).await.unwrap(),
            Outcome::Failed
        );
        assert_eq!(misskey.reactions().last().unwrap().1, "⚠️");
        assert!(misskey.texts().is_empty());
    }

    #[tokio::test]
    async fn fortune_is_deterministic_per_day_and_user_and_uses_the_categories() {
        let misskey = FakeMisskey::new();
        let cfg = config(RequireFollowed::None, false, true);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        assert_eq!(
            responder.handle(&mention("今日の運勢は？")).await.unwrap(),
            Outcome::Replied
        );
        assert_eq!(
            responder
                .handle(&mention("おみくじ引きたい"))
                .await
                .unwrap(),
            Outcome::Replied
        );
        let mut other = mention("占って");
        other.user.id = "u2".to_owned();
        responder.handle(&other).await.unwrap();
        let created = misskey.created();
        assert_eq!(
            created[0].note.text, created[1].note.text,
            "same day and user"
        );
        assert_eq!(created[0].visibility, Visibility::Followers);
        for c in &created {
            let line = c.note.text.lines().nth(1).unwrap();
            assert!(
                line == ":cat:吉" || line == ":cat:凶" || line == ":dog:吉" || line == ":dog:凶",
                "{line}"
            );
            assert!(c.note.text.starts_with("今日のあなたの運勢は\n"));
        }
    }

    #[tokio::test]
    async fn fortune_without_emojis_in_its_categories_is_a_failure() {
        let misskey = FakeMisskey::new();
        let mut cfg = config(RequireFollowed::None, false, true);
        cfg.fortune.as_mut().unwrap().categories = vec!["nope".to_owned()];
        // 一覧はあるのに、カテゴリに合う絵文字が無い。
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        assert!(matches!(
            responder.fortune(&mention("占って")).await,
            Some(Err(HandlerError::NoEmojisInCategories))
        ));
        // 一覧がまだ無ければ、一覧が空のエラー。
        let responder = Responder::new(&misskey, &cfg, SharedEmojiSet::default(), clock);
        assert!(matches!(
            responder.fortune(&mention("占って")).await,
            Some(Err(HandlerError::NoEmojis))
        ));
    }

    #[test]
    fn draws_are_stable() {
        assert_eq!(draws("20261023", "u1"), draws("20261023", "u1"));
        assert_ne!(draws("20261023", "u1"), draws("20261024", "u1"));
        assert_ne!(draws("20261023", "u1"), draws("20261023", "u2"));
    }

    #[tokio::test]
    async fn bots_and_unfollowed_users_are_ignored() {
        let misskey = FakeMisskey::new().with_follower("friend");
        let cfg = config(RequireFollowed::Remote, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let mut bot = mention("ガチャ");
        bot.user.is_bot = true;
        assert_eq!(responder.handle(&bot).await.unwrap(), Outcome::Ignored);

        let mut remote = mention("ガチャ");
        remote.user.host = Some("example.com".to_owned());
        assert_eq!(responder.handle(&remote).await.unwrap(), Outcome::Ignored);
        remote.user.id = "friend".to_owned();
        assert_eq!(responder.handle(&remote).await.unwrap(), Outcome::Replied);
        // remote なら、ローカルのユーザーはフォローの有無を見ない。
        assert_eq!(
            responder.handle(&mention("ガチャ")).await.unwrap(),
            Outcome::Replied
        );

        let cfg = config(RequireFollowed::All, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        assert_eq!(
            responder.handle(&mention("ガチャ")).await.unwrap(),
            Outcome::Ignored
        );
        assert!(misskey.reactions().is_empty());
    }

    #[tokio::test]
    async fn a_specified_mention_gets_a_specified_reply_and_a_failed_reply_is_marked() {
        let misskey = FakeMisskey::new();
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let mut direct = mention("ガチャ");
        direct.is_specified = true;
        responder.handle(&direct).await.unwrap();
        assert_eq!(misskey.created()[0].visibility, Visibility::Specified);

        misskey.set_failing(Some(Fail::Create));
        assert_eq!(
            responder.handle(&mention("ガチャ")).await.unwrap(),
            Outcome::Failed
        );
        assert_eq!(misskey.reactions(), [("m1".to_owned(), "⚠️".to_owned())]);
    }

    #[tokio::test]
    async fn run_answers_until_the_stream_closes() {
        let misskey = FakeMisskey::new();
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let source = FakeMentions(VecDeque::from([
            Ok(Event::Mention(note("m1", "ガチャ 2", Utc::now()))),
            Err(Error::InvalidRequest("broken event".into())),
            Ok(Event::Mention(note("m2", "hello", Utc::now()))),
        ]));
        run(
            source,
            &responder,
            clock().to_utc(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(misskey.texts().len(), 1);
        assert_eq!(misskey.reactions().len(), 1);
    }

    /// `minutes` 分前（時計の時刻から）の、本文が「hello」のメンション。応答は ❓ のリアクションになる。
    fn hello(id: &str, minutes: i64) -> Note {
        note(id, "hello", clock().to_utc() - TimeDelta::minutes(minutes))
    }

    fn reacted_ids(misskey: &FakeMisskey) -> Vec<String> {
        misskey.reactions().into_iter().map(|(id, _)| id).collect()
    }

    #[tokio::test]
    async fn mentions_missed_while_disconnected_are_answered_once_in_order() {
        let misskey = FakeMisskey::new().with_mentions(vec![hello("n2", 3), hello("n3", 2)]);
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let source = FakeMentions(VecDeque::from([
            Ok(Event::Mention(hello("n1", 4))),
            Ok(Event::Reconnected),
            // 取り直しで応答した n3 が、ストリームからも届く。
            Ok(Event::Mention(hello("n3", 2))),
        ]));
        run(
            source,
            &responder,
            clock().to_utc(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(reacted_ids(&misskey), ["n1", "n2", "n3"]);
        assert_eq!(
            misskey.mention_queries(),
            [Since::Date(clock().to_utc()), Since::Id("n1".to_owned())]
        );
    }

    #[tokio::test]
    async fn mentions_sent_while_connecting_are_answered_first() {
        let misskey = FakeMisskey::new().with_mentions(vec![hello("n1", 3)]);
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let opened_at = clock().to_utc() - TimeDelta::minutes(5);
        let source = FakeMentions(VecDeque::from([Ok(Event::Reconnected)]));
        run(source, &responder, opened_at, &CancellationToken::new()).await;
        assert_eq!(reacted_ids(&misskey), ["n1"]);
        // 最初はつなぎ始めた時刻から、つながり直したら最後に受けたメンションから取り直す。
        assert_eq!(
            misskey.mention_queries(),
            [Since::Date(opened_at), Since::Id("n1".to_owned())]
        );
    }

    #[tokio::test]
    async fn missed_mentions_older_than_30_minutes_are_not_answered() {
        let misskey = FakeMisskey::new().with_mentions(vec![hello("n2", 31), hello("n3", 29)]);
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let source = FakeMentions(VecDeque::from([
            Ok(Event::Mention(hello("n1", 40))),
            Ok(Event::Reconnected),
        ]));
        run(
            source,
            &responder,
            clock().to_utc(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(reacted_ids(&misskey), ["n1", "n3"]);
    }

    #[tokio::test]
    async fn missed_mentions_are_fetched_page_by_page() {
        let missed: Vec<Note> = (1..=150).map(|i| hello(&format!("n{i:03}"), 1)).collect();
        let misskey = FakeMisskey::new().with_mentions(missed);
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let source = FakeMentions(VecDeque::from([
            Ok(Event::Mention(hello("n000", 1))),
            Ok(Event::Reconnected),
        ]));
        run(
            source,
            &responder,
            clock().to_utc(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(misskey.reactions().len(), 151);
        assert_eq!(
            misskey.mention_queries(),
            [
                Since::Date(clock().to_utc()),
                Since::Id("n000".to_owned()),
                Since::Id("n100".to_owned())
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn missed_mentions_are_answered_after_a_failed_fetch_is_retried() {
        let misskey = FakeMisskey::new()
            .with_mentions(vec![hello("n1", 3)])
            .failing_times(Fail::Mentions, 2);
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let opened_at = clock().to_utc() - TimeDelta::minutes(5);
        let started = tokio::time::Instant::now();
        run(
            FakeMentions(VecDeque::new()),
            &responder,
            opened_at,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(reacted_ids(&misskey), ["n1"]);
        // 1 秒、2 秒と待って、同じ起点で 3 回目に取れる。
        assert_eq!(started.elapsed(), Duration::from_secs(1 + 2));
        assert_eq!(misskey.mention_queries(), vec![Since::Date(opened_at); 3]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_catch_up_that_cannot_succeed_is_given_up_and_the_stream_is_still_answered() {
        let misskey = FakeMisskey::new().failing(Fail::MentionsInvalid);
        let cfg = config(RequireFollowed::None, true, false);
        let responder = Responder::new(&misskey, &cfg, emojis(), clock);
        let source = FakeMentions(VecDeque::from([Ok(Event::Mention(hello("n1", 1)))]));
        let started = tokio::time::Instant::now();
        run(
            source,
            &responder,
            clock().to_utc(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(reacted_ids(&misskey), ["n1"]);
    }

    fn bad_gateway() -> Error {
        Error::UnexpectedResponse {
            endpoint: "streaming".to_owned(),
            status: 502,
            body: String::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_streaming_connection_is_retried_with_backoff() {
        let mut attempts = 0;
        let started = tokio::time::Instant::now();
        let source = connect(
            || {
                attempts += 1;
                let result = if attempts <= 8 {
                    Err(bad_gateway())
                } else {
                    Ok(FakeMentions(VecDeque::new()))
                };
                std::future::ready(result)
            },
            &CancellationToken::new(),
        )
        .await;
        assert!(source.is_some());
        assert_eq!(attempts, 9);
        // 1, 2, 4, 8, 16, 32 秒と、上限の 60 秒が 2 回。
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1 + 2 + 4 + 8 + 16 + 32 + 60 * 2)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_shutdown_stops_the_retry() {
        let shutdown = CancellationToken::new();
        let cancel = {
            let shutdown = shutdown.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                shutdown.cancel();
            }
        };
        let started = tokio::time::Instant::now();
        let (source, ()) = tokio::join!(
            connect(
                || std::future::ready(Err::<FakeMentions, _>(bad_gateway())),
                &shutdown
            ),
            cancel,
        );
        assert!(source.is_none());
        assert_eq!(started.elapsed(), Duration::from_secs(10));
    }
}
