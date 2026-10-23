//! Misskey へのアクセス。miapi（Misskey 2025.10.0 のバインディング。2026.5.0 までのサーバを読める）を使う。
//!
//! ジョブと mention は [`Misskey`] トレイト越しに使い、テストでは `fake::FakeMisskey` に差し替える。
//! メンションのストリームは、REST と寿命が違うので [`MentionSource`] に分ける。

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use miapi::v2025_10_0::api::notes::create::RequestVisibility;
use miapi::v2025_10_0::api::users::show::Response as UserShowResponse;
use miapi::v2025_10_0::streaming::main;
use miapi::v2025_10_0::{Client, types};
use tracing::{info, warn};
use url::Url;

use crate::config::Visibility;

pub use miapi::Error;

/// 時間を置けば通りうる失敗か。通信の失敗、サーバの 5xx、レートリミット。
pub fn is_transient(error: &Error) -> bool {
    match error {
        Error::Transport(_) => true,
        Error::Api(error) => {
            error.status == 429 || error.status >= 500 || error.code == "RATE_LIMIT_EXCEEDED"
        }
        Error::UnexpectedResponse { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}

/// カスタム絵文字。`emojis` が返す項目の必要なところだけ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emoji {
    pub name: String,
    pub category: Option<String>,
    pub aliases: Vec<String>,
    pub url: String,
}

/// bot が扱うノートの、必要なところだけ。
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub id: String,
    /// 本文。本文の無いノート（ファイルだけ等）は空。
    pub text: String,
    pub cw: Option<String>,
    pub user: NoteUser,
    pub created_at: DateTime<Utc>,
    /// 公開範囲が `specified` か。返信の公開範囲を決めるのに使う。
    pub is_specified: bool,
    /// リアクションの絵文字（`:name@.:` や Unicode）ごとの数。
    pub reactions: HashMap<String, u64>,
}

/// ノートの投稿者の、必要なところだけ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteUser {
    pub id: String,
    /// リモートのユーザーのホスト。ローカルなら `None`。
    pub host: Option<String>,
    pub is_bot: bool,
}

/// これから作るノート。
#[derive(Debug, Clone, Copy)]
pub struct NewNote<'a> {
    pub text: &'a str,
    pub visibility: Visibility,
    /// 返信先。
    pub reply_id: Option<&'a str>,
}

/// [`Misskey::local_timeline`] と [`Misskey::my_notes`] の起点。この時刻か ID より前のものを返す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Until {
    Date(DateTime<Utc>),
    Id(String),
}

/// プロフィールのバナーに使っているドライブのファイル。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerFile {
    pub id: String,
    pub name: String,
}

/// bot が Misskey に対して行う操作。
pub trait Misskey {
    /// ローカルのカスタム絵文字の一覧。
    async fn emojis(&self) -> Result<Vec<Emoji>, Error>;
    async fn create_note(&self, note: NewNote<'_>) -> Result<Note, Error>;
    async fn react(&self, note_id: &str, reaction: &str) -> Result<(), Error>;
    /// `user_id` が自分をフォローしているか。分からなければ `false`。
    async fn is_followed_by(&self, user_id: &str) -> Result<bool, Error>;
    /// `until` より前のローカルタイムラインのノート。新しい順に `limit` 件まで。
    async fn local_timeline(&self, until: &Until, limit: u8) -> Result<Vec<Note>, Error>;
    /// `until` より前の自分のノート。新しい順に `limit` 件まで。他人への返信を含み、リノートは含めない。
    async fn my_notes(&self, until: &Until, limit: u8) -> Result<Vec<Note>, Error>;
    /// `since` より後の、自分へのメンション。古い順に `limit` 件まで。
    async fn mentions_since(&self, since: &Since, limit: u8) -> Result<Vec<Note>, Error>;
    /// ドライブにファイルを上げ、その ID を返す。中身が既存のファイルと同じでも、新しいファイルにする。
    async fn upload_file(
        &self,
        name: &str,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<String, Error>;
    /// 今のバナーのファイル。設定していないか、ファイルが消えていれば `None`。
    async fn current_banner(&self) -> Result<Option<BannerFile>, Error>;
    /// `file_id` のファイルをバナーにする。`None` ならバナーを外す。
    async fn set_banner(&self, file_id: Option<&str>) -> Result<(), Error>;
    async fn delete_file(&self, file_id: &str) -> Result<(), Error>;
}

/// [`Misskey::mentions_since`] の起点。この ID か時刻より後のものを返す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Since {
    Id(String),
    Date(DateTime<Utc>),
}

/// メンションの受け口から届くもの。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Mention(Note),
    /// 切れていたストリーミングがつながり直した。切れていた間のメンションは届かないので、REST で取り直す。
    Reconnected,
}

/// 自分へのメンションを順に受け取る。
pub trait MentionSource {
    /// 次のイベント。ストリームが閉じたら `None`。デコードできないイベントは `Err` を 1 件返して続く。
    async fn recv(&mut self) -> Option<Result<Event, Error>>;
}

/// miapi で Misskey を叩く実装。
#[derive(Debug)]
pub struct MiapiMisskey {
    client: Client,
    /// 自分のユーザー ID。`users/notes` で自分のノートを引くのに使う。
    user_id: String,
}

impl MiapiMisskey {
    /// `http` で接続し、`i` を呼んでトークンを確かめる。不正なトークンならここで失敗する。
    ///
    /// `http` は `http1_only()` で作ること。ストリーミング（WebSocket の upgrade）は HTTP/1.1 でしか成立しない。
    pub async fn connect(server: &Url, token: &str, http: reqwest::Client) -> Result<Self, Error> {
        let client = Client::with_http_client(http, server.as_str(), Some(token))?;
        let me = client.i().await?;
        info!(username = %me.username, user_id = %me.id, "connected to Misskey");
        Ok(Self {
            client,
            user_id: me.id,
        })
    }

    /// ストリーミングの `main` チャンネルにつなぎ、メンションの受け口を返す。
    pub async fn mentions(&self) -> Result<MentionStream, Error> {
        let stream = self.client.streaming().await?;
        let connection = stream.main().await?;
        info!("connected to the streaming main channel");
        Ok(MentionStream {
            connection,
            state: stream.state(),
        })
    }
}

impl Misskey for MiapiMisskey {
    async fn emojis(&self) -> Result<Vec<Emoji>, Error> {
        let response = self.client.emojis().await?;
        Ok(response.emojis.into_iter().map(Emoji::from).collect())
    }

    async fn create_note(&self, note: NewNote<'_>) -> Result<Note, Error> {
        // `specified` は宛先（visibleUserIds）を送らない。返信なら Misskey が返信先のユーザーを宛先に足す。
        let visibility = match note.visibility {
            Visibility::Public => RequestVisibility::Public,
            Visibility::Home => RequestVisibility::Home,
            Visibility::Followers => RequestVisibility::Followers,
            Visibility::Specified => RequestVisibility::Specified,
        };
        let response = self
            .client
            .notes()
            .create()
            .visibility(visibility)
            .text(note.text)
            .maybe_reply_id(note.reply_id)
            .await?;
        Ok(response.created_note.into())
    }

    async fn react(&self, note_id: &str, reaction: &str) -> Result<(), Error> {
        self.client
            .notes()
            .reactions()
            .create()
            .note_id(note_id)
            .reaction(reaction)
            .await?;
        Ok(())
    }

    async fn is_followed_by(&self, user_id: &str) -> Result<bool, Error> {
        let response = self.client.users().show().user_id(user_id).await?;
        Ok(match response {
            UserShowResponse::UserDetailedNotMe(user) => user.is_followed.unwrap_or(false),
            // 自分自身、複数指定の応答、知らない形は「フォローされていない」として扱う（旧版と同じく、不明なら無視する）。
            UserShowResponse::MeDetailed(_)
            | UserShowResponse::Array(_)
            | UserShowResponse::Unrecognized(_) => false,
        })
    }

    async fn local_timeline(&self, until: &Until, limit: u8) -> Result<Vec<Note>, Error> {
        let call = self.client.notes().local_timeline().limit(i64::from(limit));
        let call = match until {
            Until::Date(date) => call.until_date(date.timestamp_millis()),
            Until::Id(id) => call.until_id(id.as_str()),
        };
        Ok(call.await?.into_iter().map(Note::from).collect())
    }

    async fn my_notes(&self, until: &Until, limit: u8) -> Result<Vec<Note>, Error> {
        // 返信も含めて頼む。Misskey（2026.3.2）は DB から引く経路で withReplies=false を見ないので、
        // false では経路によって結果が変わる。
        let call = self
            .client
            .users()
            .notes()
            .user_id(&self.user_id)
            .with_replies(true)
            .with_renotes(false)
            .limit(i64::from(limit));
        let call = match until {
            Until::Date(date) => call.until_date(date.timestamp_millis()),
            Until::Id(id) => call.until_id(id.as_str()),
        };
        Ok(call.await?.into_iter().map(Note::from).collect())
    }

    async fn mentions_since(&self, since: &Since, limit: u8) -> Result<Vec<Note>, Error> {
        // `since_id` か `since_date` を渡すと、Misskey はそれより後のノートを古い順に返す。
        let call = self.client.notes().mentions().limit(i64::from(limit));
        let call = match since {
            Since::Id(id) => call.since_id(id.as_str()),
            Since::Date(date) => call.since_date(date.timestamp_millis()),
        };
        Ok(call.await?.into_iter().map(Note::from).collect())
    }

    async fn upload_file(
        &self,
        name: &str,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<String, Error> {
        // force を付けないと、Misskey は同じ中身（md5）の既存ファイルを返す。前のバナーと同じ ID が返ると、
        // 前のバナーとして消してしまう。
        let file = self
            .client
            .drive()
            .files()
            .create()
            .name(name)
            .force(true)
            .file(miapi::FilePart {
                file_name: name.to_owned(),
                content_type: Some(content_type.to_owned()),
                data,
            })
            .await?;
        Ok(file.id)
    }

    async fn current_banner(&self) -> Result<Option<BannerFile>, Error> {
        let me = self.client.i().await?;
        let Some(id) = me.banner_id else {
            return Ok(None);
        };
        match self.client.drive().files().show().file_id(&id).await {
            Ok(file) => Ok(Some(BannerFile {
                id: file.id,
                name: file.name,
            })),
            Err(Error::Api(error)) if error.code == "NO_SUCH_FILE" => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn set_banner(&self, file_id: Option<&str>) -> Result<(), Error> {
        let update = self.client.i().update();
        match file_id {
            Some(file_id) => update.banner_id(file_id),
            None => update.banner_id_null(),
        }
        .await?;
        Ok(())
    }

    async fn delete_file(&self, file_id: &str) -> Result<(), Error> {
        self.client
            .drive()
            .files()
            .delete()
            .file_id(file_id)
            .await?;
        Ok(())
    }
}

/// ストリーミングの `main` チャンネル。切れたら miapi がつなぎ直す。
pub struct MentionStream {
    connection: main::Connection,
    state: tokio::sync::watch::Receiver<miapi::stream::State>,
}

impl MentionSource for MentionStream {
    async fn recv(&mut self) -> Option<Result<Event, Error>> {
        loop {
            // 状態が変わって `Connected` なら、つなぎ直した後である。その接続のメンションより先に `Reconnected` を返すよう、
            // 状態の変化を先に見る（miapi は `Connected` にしてからイベントを振り分ける。`Stream::state` の doc）。
            let event = tokio::select! {
                biased;
                changed = self.state.changed() => {
                    // 送り手が無くなったら、ストリームは閉じている。
                    changed.ok()?;
                    match &*self.state.borrow_and_update() {
                        miapi::stream::State::Connected => {
                            info!("the streaming connection is back");
                            return Some(Ok(Event::Reconnected));
                        }
                        // トークンの失効などに気付くよう、つなぎ直しに失敗するたびに warn する。
                        miapi::stream::State::Reconnecting { attempts, last_error } => {
                            warn!(attempts, error = %last_error, "the streaming connection is down; reconnecting");
                        }
                        miapi::stream::State::Closed => {}
                    }
                    continue;
                }
                event = self.connection.recv() => event?,
            };
            match event {
                Ok(main::Event::Mention(note)) => return Some(Ok(Event::Mention(note.into()))),
                Ok(_) => {}
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

impl From<types::EmojiSimple> for Emoji {
    fn from(emoji: types::EmojiSimple) -> Self {
        Self {
            name: emoji.name,
            category: emoji.category,
            aliases: emoji.aliases,
            url: emoji.url,
        }
    }
}

/// miapi の `Note` から、bot が使うところだけを取り出す。
impl From<types::Note> for Note {
    fn from(note: types::Note) -> Self {
        Self {
            id: note.id,
            text: note.text.unwrap_or_default(),
            cw: note.cw,
            user: NoteUser {
                id: note.user.id,
                host: note.user.host,
                is_bot: note.user.is_bot.unwrap_or(false),
            },
            created_at: note.created_at,
            is_specified: matches!(note.visibility, types::NoteVisibility::Specified),
            // Misskey の JSON の数は整数だが、spec の `number` は f64 になる。
            reactions: note
                .reactions
                .into_iter()
                .map(|(key, count)| (key, count.round().max(0.0) as u64))
                .collect(),
        }
    }
}

/// ジョブのテストで使う、メモリ上の Misskey。
#[cfg(test)]
pub mod fake {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Mutex;

    use chrono::{DateTime, TimeDelta, Utc};

    use super::{
        BannerFile, Emoji, Error, Event, MentionSource, Misskey, NewNote, Note, NoteUser, Since,
        Until,
    };
    use crate::config::Visibility;
    use crate::mfm::{MAX_NOTE_LENGTH, note_length};

    /// 失敗させたい操作。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Fail {
        Emojis,
        Create,
        React,
        Timeline,
        Upload,
        SetBanner,
        /// 投稿を INVALID_PARAM で拒む。
        CreateInvalid,
        /// 投稿は保存したのに失敗を返す（応答の前に通信が切れた場合）。
        CreateStored,
        /// メンションの取得を INTERNAL_ERROR（一時的な失敗）で失敗させる。
        Mentions,
        /// メンションの取得を INVALID_PARAM で拒む。
        MentionsInvalid,
    }

    /// 作ったノート。
    #[derive(Debug, Clone, PartialEq)]
    pub struct Created {
        pub note: Note,
        pub visibility: Visibility,
    }

    /// ドライブのファイル。
    #[derive(Debug, Clone)]
    struct File {
        id: String,
        name: String,
    }

    #[derive(Debug, Default)]
    struct State {
        emojis: Vec<Emoji>,
        /// 自分が作ったノート。作成順。
        created: Vec<Created>,
        /// ローカルタイムラインのノート。新しい順に並べて返す。
        timeline: Vec<Note>,
        /// 送ったリアクション（ノート ID、絵文字）。
        reactions: Vec<(String, String)>,
        /// 自分をフォローしているユーザーの ID。
        followers: HashSet<String>,
        files: Vec<File>,
        /// 次に上げるファイルの通し番号。削除しても ID は重ならない。
        next_file: u64,
        /// `upload_file` が呼ばれた回数。
        uploads: usize,
        banner_id: Option<String>,
        next_id: u64,
        fail: Option<Fail>,
        /// `fail` を失敗させる残りの回数。`None` なら何度でも失敗させる。
        fail_times: Option<u32>,
        /// 応答を返さずに止まる操作。
        hang: Option<Fail>,
        base_time: DateTime<Utc>,
        /// `local_timeline` が呼ばれた起点。
        timeline_queries: Vec<Until>,
        /// `mentions_since` が返す、自分へのメンション。
        mentions: Vec<Note>,
        /// `mentions_since` が呼ばれた起点。
        mention_queries: Vec<Since>,
    }

    #[derive(Debug, Default)]
    pub struct FakeMisskey {
        state: Mutex<State>,
    }

    pub fn user(id: &str) -> NoteUser {
        NoteUser {
            id: id.to_owned(),
            host: None,
            is_bot: false,
        }
    }

    pub fn note(id: &str, text: &str, created_at: DateTime<Utc>) -> Note {
        Note {
            id: id.to_owned(),
            text: text.to_owned(),
            cw: None,
            user: user("u1"),
            created_at,
            is_specified: false,
            reactions: HashMap::new(),
        }
    }

    fn api_error(endpoint: &str, code: &str) -> Error {
        // Misskey と同じ HTTP の状態を付ける。
        let status = match code {
            "INTERNAL_ERROR" => 500,
            "RATE_LIMIT_EXCEEDED" => 429,
            _ => 400,
        };
        Error::Api(Box::new(miapi::ApiError {
            status,
            endpoint: endpoint.to_owned(),
            id: "00000000-0000-0000-0000-000000000000".to_owned(),
            code: code.to_owned(),
            message: format!("fake error {code}"),
            kind: None,
            info: None,
        }))
    }

    /// `notes` のうち `until` より前のものを、新しい順に `limit` 件まで。fake の ID は `note10` が `note9` より前に
    /// 並ぶので、ID は文字列で比べず位置で切る。
    fn before(mut notes: Vec<Note>, until: &Until, limit: u8) -> Vec<Note> {
        notes.sort_by_key(|note| std::cmp::Reverse(note.created_at));
        match until {
            Until::Date(date) => notes.retain(|note| note.created_at < *date),
            Until::Id(id) => match notes.iter().position(|note| &note.id == id) {
                Some(index) => drop(notes.drain(..=index)),
                None => notes.clear(),
            },
        }
        notes.truncate(usize::from(limit));
        notes
    }

    impl FakeMisskey {
        pub fn new() -> Self {
            Self {
                state: Mutex::new(State {
                    base_time: Utc::now() - TimeDelta::days(1),
                    ..State::default()
                }),
            }
        }

        /// 作るノートの作成日時の起点。
        pub fn with_base_time(self, base_time: DateTime<Utc>) -> Self {
            self.state.lock().unwrap().base_time = base_time;
            self
        }

        pub fn with_emojis(self, emojis: Vec<Emoji>) -> Self {
            self.state.lock().unwrap().emojis = emojis;
            self
        }

        pub fn set_emojis(&self, emojis: Vec<Emoji>) {
            self.state.lock().unwrap().emojis = emojis;
        }

        pub fn with_timeline(self, notes: Vec<Note>) -> Self {
            self.state.lock().unwrap().timeline = notes;
            self
        }

        /// `mentions_since` で返すメンション。
        pub fn with_mentions(self, notes: Vec<Note>) -> Self {
            self.state.lock().unwrap().mentions = notes;
            self
        }

        pub fn mention_queries(&self) -> Vec<Since> {
            self.state.lock().unwrap().mention_queries.clone()
        }

        pub fn with_follower(self, user_id: &str) -> Self {
            self.state
                .lock()
                .unwrap()
                .followers
                .insert(user_id.to_owned());
            self
        }

        /// 既にあるドライブのファイルをバナーにしておく。
        pub fn with_banner(self, file_id: &str, name: &str) -> Self {
            let mut state = self.state.lock().unwrap();
            state.files.push(File {
                id: file_id.to_owned(),
                name: name.to_owned(),
            });
            state.banner_id = Some(file_id.to_owned());
            drop(state);
            self
        }

        /// 既に投稿した自分のノートを足す。
        pub fn with_own_note(self, text: &str) -> Self {
            self.state.lock().unwrap().push(text, Visibility::Home);
            self
        }

        /// 指定した操作を失敗させる。
        pub fn failing(self, fail: Fail) -> Self {
            self.state.lock().unwrap().fail = Some(fail);
            self
        }

        pub fn set_failing(&self, fail: Option<Fail>) {
            self.state.lock().unwrap().fail = fail;
        }

        /// 指定した操作を `times` 回だけ失敗させ、その後は成功させる。
        pub fn failing_times(self, fail: Fail, times: u32) -> Self {
            let mut state = self.state.lock().unwrap();
            state.fail = Some(fail);
            state.fail_times = Some(times);
            drop(state);
            self
        }

        /// 指定した操作を、応答を返さずに止める。timeout で打ち切られる場面を作るのに使う。
        pub fn set_hanging(&self, hang: Option<Fail>) {
            self.state.lock().unwrap().hang = hang;
        }

        async fn hang_if(&self, operation: Fail) {
            let hang = self.state.lock().unwrap().hang;
            if hang == Some(operation) {
                std::future::pending::<()>().await;
            }
        }

        /// 作ったノート。作成順。
        pub fn created(&self) -> Vec<Created> {
            self.state.lock().unwrap().created.clone()
        }

        /// 作ったノートの本文。作成順。
        pub fn texts(&self) -> Vec<String> {
            self.created().into_iter().map(|c| c.note.text).collect()
        }

        pub fn reactions(&self) -> Vec<(String, String)> {
            self.state.lock().unwrap().reactions.clone()
        }

        /// ドライブのファイルの名前。残っているものだけ。
        pub fn file_names(&self) -> Vec<String> {
            self.state
                .lock()
                .unwrap()
                .files
                .iter()
                .map(|file| file.name.clone())
                .collect()
        }

        /// `upload_file` が呼ばれた回数。
        pub fn uploads(&self) -> usize {
            self.state.lock().unwrap().uploads
        }

        pub fn banner_id(&self) -> Option<String> {
            self.state.lock().unwrap().banner_id.clone()
        }

        pub fn timeline_queries(&self) -> Vec<Until> {
            self.state.lock().unwrap().timeline_queries.clone()
        }
    }

    impl State {
        /// `operation` を失敗させるか。回数を決めていれば、その回数だけ失敗させる。
        fn should_fail(&mut self, operation: Fail) -> bool {
            if self.fail != Some(operation) {
                return false;
            }
            match &mut self.fail_times {
                None => true,
                Some(0) => false,
                Some(times) => {
                    *times -= 1;
                    true
                }
            }
        }

        fn push(&mut self, text: &str, visibility: Visibility) -> Note {
            let note = Note {
                id: format!("note{}", self.next_id),
                text: text.to_owned(),
                cw: None,
                user: user("me"),
                created_at: self.base_time + TimeDelta::minutes(self.next_id as i64),
                is_specified: visibility == Visibility::Specified,
                reactions: HashMap::new(),
            };
            self.next_id += 1;
            self.created.push(Created {
                note: note.clone(),
                visibility,
            });
            note
        }
    }

    impl Misskey for FakeMisskey {
        async fn emojis(&self) -> Result<Vec<Emoji>, Error> {
            let mut state = self.state.lock().unwrap();
            if state.should_fail(Fail::Emojis) {
                return Err(api_error("emojis", "INTERNAL_ERROR"));
            }
            Ok(state.emojis.clone())
        }

        async fn create_note(&self, note: NewNote<'_>) -> Result<Note, Error> {
            self.hang_if(Fail::Create).await;
            let mut state = self.state.lock().unwrap();
            if state.should_fail(Fail::Create) {
                return Err(api_error("notes/create", "RATE_LIMIT_EXCEEDED"));
            }
            // サーバの上限を超える本文は、Misskey と同じく INVALID_PARAM で拒む。
            if state.should_fail(Fail::CreateInvalid) || note_length(note.text) > MAX_NOTE_LENGTH {
                return Err(api_error("notes/create", "INVALID_PARAM"));
            }
            let created = state.push(note.text, note.visibility);
            if state.should_fail(Fail::CreateStored) {
                return Err(api_error("notes/create", "INTERNAL_ERROR"));
            }
            Ok(created)
        }

        async fn react(&self, note_id: &str, reaction: &str) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            if state.should_fail(Fail::React) {
                return Err(api_error("notes/reactions/create", "INTERNAL_ERROR"));
            }
            state
                .reactions
                .push((note_id.to_owned(), reaction.to_owned()));
            Ok(())
        }

        async fn is_followed_by(&self, user_id: &str) -> Result<bool, Error> {
            Ok(self.state.lock().unwrap().followers.contains(user_id))
        }

        async fn local_timeline(&self, until: &Until, limit: u8) -> Result<Vec<Note>, Error> {
            let mut state = self.state.lock().unwrap();
            if state.should_fail(Fail::Timeline) {
                return Err(api_error("notes/local-timeline", "INTERNAL_ERROR"));
            }
            state.timeline_queries.push(until.clone());
            Ok(before(state.timeline.clone(), until, limit))
        }

        async fn my_notes(&self, until: &Until, limit: u8) -> Result<Vec<Note>, Error> {
            let state = self.state.lock().unwrap();
            let notes = state.created.iter().map(|c| c.note.clone()).collect();
            Ok(before(notes, until, limit))
        }

        async fn mentions_since(&self, since: &Since, limit: u8) -> Result<Vec<Note>, Error> {
            let mut state = self.state.lock().unwrap();
            // 失敗した回の起点も残し、同じ起点で試し直したかを確かめられるようにする。
            state.mention_queries.push(since.clone());
            if state.should_fail(Fail::Mentions) {
                return Err(api_error("notes/mentions", "INTERNAL_ERROR"));
            }
            if state.should_fail(Fail::MentionsInvalid) {
                return Err(api_error("notes/mentions", "INVALID_PARAM"));
            }
            let mut notes: Vec<Note> = state
                .mentions
                .iter()
                .filter(|note| match since {
                    Since::Id(id) => note.id > *id,
                    Since::Date(date) => note.created_at > *date,
                })
                .cloned()
                .collect();
            notes.sort_by(|a, b| a.id.cmp(&b.id));
            notes.truncate(usize::from(limit));
            Ok(notes)
        }

        async fn upload_file(
            &self,
            name: &str,
            _content_type: &str,
            _data: Vec<u8>,
        ) -> Result<String, Error> {
            let mut state = self.state.lock().unwrap();
            state.uploads += 1;
            if state.should_fail(Fail::Upload) {
                return Err(api_error("drive/files/create", "INTERNAL_ERROR"));
            }
            let id = format!("file{}", state.next_file);
            state.next_file += 1;
            state.files.push(File {
                id: id.clone(),
                name: name.to_owned(),
            });
            Ok(id)
        }

        async fn current_banner(&self) -> Result<Option<BannerFile>, Error> {
            let state = self.state.lock().unwrap();
            Ok(state.banner_id.as_ref().and_then(|id| {
                state
                    .files
                    .iter()
                    .find(|file| &file.id == id)
                    .map(|file| BannerFile {
                        id: file.id.clone(),
                        name: file.name.clone(),
                    })
            }))
        }

        async fn set_banner(&self, file_id: Option<&str>) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            if state.should_fail(Fail::SetBanner) {
                return Err(api_error("i/update", "INTERNAL_ERROR"));
            }
            if let Some(file_id) = file_id
                && !state.files.iter().any(|file| file.id == file_id)
            {
                return Err(api_error("i/update", "NO_SUCH_FILE"));
            }
            state.banner_id = file_id.map(str::to_owned);
            Ok(())
        }

        async fn delete_file(&self, file_id: &str) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            let before = state.files.len();
            state.files.retain(|file| file.id != file_id);
            if state.files.len() == before {
                return Err(api_error("drive/files/delete", "NO_SUCH_FILE"));
            }
            Ok(())
        }
    }

    /// 決まったイベントを順に返す受け口。尽きたら `None`。
    pub struct FakeMentions(pub VecDeque<Result<Event, Error>>);

    impl MentionSource for FakeMentions {
        async fn recv(&mut self) -> Option<Result<Event, Error>> {
            self.0.pop_front()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn file_ids_are_not_reused_after_a_delete() {
            let misskey = FakeMisskey::new();
            let first = misskey
                .upload_file("a", "image/png", vec![1])
                .await
                .unwrap();
            misskey.delete_file(&first).await.unwrap();
            let second = misskey
                .upload_file("b", "image/png", vec![2])
                .await
                .unwrap();
            assert_ne!(first, second);
            assert_eq!(misskey.uploads(), 2);
        }

        #[tokio::test]
        async fn a_note_over_the_length_limit_is_rejected() {
            let misskey = FakeMisskey::new();
            fn note(text: &str) -> NewNote<'_> {
                NewNote {
                    text,
                    visibility: Visibility::Home,
                    reply_id: None,
                }
            }
            // 🥇 は UTF-16 で 2 単位なので、1500 個で上限ちょうどになる。
            let fits = "🥇".repeat(MAX_NOTE_LENGTH / 2);
            misskey.create_note(note(&fits)).await.unwrap();
            let over = format!("{fits}a");
            let error = misskey.create_note(note(&over)).await.unwrap_err();
            assert!(error.to_string().contains("INVALID_PARAM"), "{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wiremock::matchers::{body_json, body_string_contains, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const NOTE: &str = include_str!("../tests/fixtures/misskey/note.json");
    const ME: &str = include_str!("../tests/fixtures/misskey/i.json");
    const EMOJIS: &str = include_str!("../tests/fixtures/misskey/emojis.json");

    fn note_json() -> Value {
        serde_json::from_str(NOTE).unwrap()
    }

    fn me_json() -> Value {
        serde_json::from_str(ME).unwrap()
    }

    /// `i` に応答するモックサーバと、それにつないだクライアント。
    async fn connected() -> (MockServer, MiapiMisskey) {
        let server = MockServer::start().await;
        Mock::given(path("/api/i"))
            .and(body_json(json!({"i": "tok"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_json()))
            .mount(&server)
            .await;
        let misskey = MiapiMisskey::connect(
            &server.uri().parse().unwrap(),
            "tok",
            reqwest::Client::builder().http1_only().build().unwrap(),
        )
        .await
        .unwrap();
        (server, misskey)
    }

    #[tokio::test]
    async fn connect_verifies_the_token_and_remembers_the_user_id() {
        let (_server, misskey) = connected().await;
        assert_eq!(misskey.user_id, "as4pl7oasexx0004");
    }

    #[tokio::test]
    async fn connect_fails_with_a_bad_token() {
        let server = MockServer::start().await;
        Mock::given(path("/api/i"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": {"message": "Authentication failed.", "code": "AUTHENTICATION_FAILED", "id": "b0a7f5f8-dc2f-4171-b91f-de88ad238e14"}
            })))
            .mount(&server)
            .await;
        let error = MiapiMisskey::connect(
            &server.uri().parse().unwrap(),
            "bad",
            reqwest::Client::new(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, Error::Api(ref e) if e.code == "AUTHENTICATION_FAILED"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn emojis_are_read_from_the_list() {
        let (server, misskey) = connected().await;
        Mock::given(path("/api/emojis"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::from_str::<Value>(EMOJIS).unwrap()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let emojis = misskey.emojis().await.unwrap();
        assert_eq!(emojis.len(), 2);
        assert_eq!(emojis[0].name, "emoji1");
        assert_eq!(emojis[0].category.as_deref(), Some("category1"));
        assert_eq!(emojis[0].aliases, ["alias1"]);
        assert_eq!(emojis[1].category, None);
        assert!(emojis[1].aliases.is_empty());
    }

    #[tokio::test]
    async fn create_note_sends_visibility_text_and_reply_id() {
        let (server, misskey) = connected().await;
        Mock::given(path("/api/notes/create"))
            .and(body_json(
                json!({"visibility": "specified", "text": "hello", "replyId": "n0", "i": "tok"}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"createdNote": note_json()})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let note = misskey
            .create_note(NewNote {
                text: "hello",
                visibility: Visibility::Specified,
                reply_id: Some("n0"),
            })
            .await
            .unwrap();
        assert_eq!(note.id, note_json()["id"].as_str().unwrap());
        assert!(!note.user.is_bot);
    }

    #[tokio::test]
    async fn react_sends_the_note_id_and_the_reaction() {
        let (server, misskey) = connected().await;
        Mock::given(path("/api/notes/reactions/create"))
            .and(body_json(
                json!({"noteId": "n1", "reaction": "❓", "i": "tok"}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        misskey.react("n1", "❓").await.unwrap();
    }

    #[tokio::test]
    async fn is_followed_by_reads_the_flag_and_treats_other_shapes_as_false() {
        let (server, misskey) = connected().await;
        let mut user = me_json();
        user["isFollowed"] = json!(true);
        user["isFollowing"] = json!(false);
        // MeDetailed にしか無いフィールドを外し、UserDetailedNotMe として読ませる。
        for key in [
            "email",
            "emailVerified",
            "hasUnreadAnnouncement",
            "policies",
        ] {
            user.as_object_mut().unwrap().remove(key);
        }
        Mock::given(path("/api/users/show"))
            .and(body_json(json!({"userId": "u1", "i": "tok"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(user))
            .mount(&server)
            .await;
        Mock::given(path("/api/users/show"))
            .and(body_json(json!({"userId": "me", "i": "tok"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_json()))
            .mount(&server)
            .await;
        assert!(misskey.is_followed_by("u1").await.unwrap());
        assert!(!misskey.is_followed_by("me").await.unwrap());
    }

    #[tokio::test]
    async fn local_timeline_sends_the_page_and_reads_reactions() {
        let (server, misskey) = connected().await;
        let mut note = note_json();
        note["reactions"] = json!({":a@.:": 3, "👍": 1});
        Mock::given(path("/api/notes/local-timeline"))
            .and(body_json(
                json!({"untilDate": 1700000000000i64, "limit": 100, "i": "tok"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([note])))
            .expect(1)
            .mount(&server)
            .await;
        let until = Until::Date(DateTime::from_timestamp_millis(1_700_000_000_000).unwrap());
        let notes = misskey.local_timeline(&until, 100).await.unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].reactions[":a@.:"], 3);
        assert_eq!(notes[0].reactions["👍"], 1);
    }

    #[tokio::test]
    async fn mentions_since_queries_by_id_or_date() {
        let (server, misskey) = connected().await;
        Mock::given(path("/api/notes/mentions"))
            .and(body_json(
                json!({"limit": 100, "sinceId": "n1", "i": "tok"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([note_json()])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/notes/mentions"))
            .and(body_json(
                json!({"limit": 100, "sinceDate": 1700000000000i64, "i": "tok"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let by_id = misskey
            .mentions_since(&Since::Id("n1".to_owned()), 100)
            .await
            .unwrap();
        assert_eq!(by_id.len(), 1);
        let by_date = misskey
            .mentions_since(
                &Since::Date(DateTime::from_timestamp_millis(1_700_000_000_000).unwrap()),
                100,
            )
            .await
            .unwrap();
        assert!(by_date.is_empty());
    }

    #[tokio::test]
    async fn my_notes_queries_own_notes_by_date_or_id() {
        let (server, misskey) = connected().await;
        Mock::given(path("/api/users/notes"))
            .and(body_json(json!({
                "userId": "as4pl7oasexx0004", "withReplies": true, "withRenotes": false, "untilDate": 1700000000000i64, "limit": 100, "i": "tok"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([note_json()])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/users/notes"))
            .and(body_json(json!({
                "userId": "as4pl7oasexx0004", "withReplies": true, "withRenotes": false, "untilId": "n1", "limit": 100, "i": "tok"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let by_date = misskey
            .my_notes(
                &Until::Date(DateTime::from_timestamp_millis(1_700_000_000_000).unwrap()),
                100,
            )
            .await
            .unwrap();
        assert_eq!(by_date.len(), 1);
        let by_id = misskey
            .my_notes(&Until::Id("n1".to_owned()), 100)
            .await
            .unwrap();
        assert!(by_id.is_empty());
    }

    #[tokio::test]
    async fn the_banner_is_read_and_replaced_through_the_drive() {
        let (server, misskey) = connected().await;
        let file = json!({
            "id": "as4pl7zxsexx000c", "createdAt": "2026-10-09T15:53:11.194Z", "name": "banner_1.png", "type": "image/png",
            "md5": "d41d8cd98f00b204e9800998ecf8427e", "size": 1, "isSensitive": false, "blurhash": null,
            "properties": {}, "url": "https://misskey.example.com/files/x", "thumbnailUrl": null, "comment": null,
            "folderId": null, "folder": null, "userId": null, "user": null
        });
        Mock::given(path("/api/drive/files/show"))
            .and(body_json(json!({"fileId": "as4pl7zxsexx000c", "i": "tok"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(file.clone()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/drive/files/create"))
            .and(body_string_contains("name=\"force\"\r\n\r\ntrue\r\n"))
            .respond_with(ResponseTemplate::new(200).set_body_json(file))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/i/update"))
            .and(body_json(
                json!({"bannerId": "as4pl7zxsexx000c", "i": "tok"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_json()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/i/update"))
            .and(body_json(json!({"bannerId": null, "i": "tok"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_json()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/drive/files/delete"))
            .and(body_json(json!({"fileId": "old", "i": "tok"})))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let banner = misskey.current_banner().await.unwrap().unwrap();
        assert_eq!(banner.name, "banner_1.png");
        let id = misskey
            .upload_file("banner_2.png", "image/png", vec![1, 2, 3])
            .await
            .unwrap();
        misskey.set_banner(Some(&id)).await.unwrap();
        misskey.set_banner(None).await.unwrap();
        misskey.delete_file("old").await.unwrap();
    }

    #[tokio::test]
    async fn a_missing_banner_file_is_none() {
        let (server, misskey) = connected().await;
        Mock::given(path("/api/drive/files/show"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": {"message": "No such file.", "code": "NO_SUCH_FILE", "id": "067bc436-2718-4795-b0fb-ecbe43949e31"}
            })))
            .mount(&server)
            .await;
        assert_eq!(misskey.current_banner().await.unwrap(), None);
    }

    /// 実サーバに対する往復。`scripts/misskey/up.sh` で立てたローカル Misskey に向けて手で実行する。
    #[tokio::test]
    #[ignore = "needs a live server: set MISSKEY_SERVER and MISSKEY_TOKEN (see scripts/misskey/up.sh)"]
    async fn live_round_trip() {
        let server: Url = std::env::var("MISSKEY_SERVER")
            .expect("MISSKEY_SERVER を設定する")
            .parse()
            .unwrap();
        let token = std::env::var("MISSKEY_TOKEN").expect("MISSKEY_TOKEN を設定する");
        let http = reqwest::Client::builder().http1_only().build().unwrap();
        let misskey = MiapiMisskey::connect(&server, &token, http).await.unwrap();

        let emojis = misskey.emojis().await.unwrap();
        assert!(!emojis.is_empty(), "up.sh registers some emojis");

        let text = format!("live_round_trip {}", Utc::now().to_rfc3339());
        let note = misskey
            .create_note(NewNote {
                text: &text,
                visibility: Visibility::Specified,
                reply_id: None,
            })
            .await
            .unwrap();
        misskey.react(&note.id, "👍").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let mine = misskey
            .my_notes(&Until::Date(Utc::now()), 10)
            .await
            .unwrap();
        assert!(mine.iter().any(|n| n.id == note.id));
        let timeline = misskey
            .local_timeline(&Until::Date(Utc::now()), 10)
            .await
            .unwrap();
        assert!(timeline.len() <= 10);

        let previous = misskey.current_banner().await.unwrap();
        let png = include_bytes!("../tests/fixtures/images/red.png").to_vec();
        let id = misskey
            .upload_file("banner_live.png", "image/png", png)
            .await
            .unwrap();
        misskey.set_banner(Some(&id)).await.unwrap();
        let banner = misskey.current_banner().await.unwrap().unwrap();
        assert_eq!(banner.id, id);
        misskey.delete_file(&id).await.unwrap();
        if let Some(previous) = previous {
            misskey.set_banner(Some(&previous.id)).await.unwrap();
        }

        // ストリーミングの upgrade が通ること。
        let _mentions = misskey.mentions().await.unwrap();
    }
}
