//! Misskey へのアクセス。miapi（Misskey 2025.10.0 のバインディング。2026.5.0 までのサーバを読める）を使う。
//!
//! ジョブは [`Misskey`] トレイト越しに使い、テストでは `fake::FakeMisskey` に差し替える。

use miapi::v2025_10_0::api::notes::create::RequestVisibility;
use miapi::v2025_10_0::{Client, types};
use tracing::info;
use url::Url;

use crate::config::Visibility;

pub use miapi::Error;

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
}

/// これから作るノート。
#[derive(Debug, Clone, Copy)]
pub struct NewNote<'a> {
    pub text: &'a str,
    pub visibility: Visibility,
}

/// bot が Misskey に対して行う操作。
pub trait Misskey {
    /// ローカルのカスタム絵文字の一覧。
    async fn emojis(&self) -> Result<Vec<Emoji>, Error>;
    async fn create_note(&self, note: NewNote<'_>) -> Result<Note, Error>;
}

/// miapi で Misskey を叩く実装。
#[derive(Debug)]
pub struct MiapiMisskey {
    client: Client,
}

impl MiapiMisskey {
    /// `http` で接続し、`i` を呼んでトークンを確かめる。不正なトークンならここで失敗する。
    ///
    /// `http` は `http1_only()` で作ること。ストリーミング（WebSocket の upgrade）は HTTP/1.1 でしか成立しない。
    pub async fn connect(server: &Url, token: &str, http: reqwest::Client) -> Result<Self, Error> {
        let client = Client::with_http_client(http, server.as_str(), Some(token))?;
        let me = client.i().await?;
        info!(username = %me.username, user_id = %me.id, "connected to Misskey");
        Ok(Self { client })
    }
}

impl Misskey for MiapiMisskey {
    async fn emojis(&self) -> Result<Vec<Emoji>, Error> {
        let response = self.client.emojis().await?;
        Ok(response.emojis.into_iter().map(Emoji::from).collect())
    }

    async fn create_note(&self, note: NewNote<'_>) -> Result<Note, Error> {
        // `specified` は宛先（visibleUserIds）を送らない。
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
            .await?;
        Ok(response.created_note.into())
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
        Self { id: note.id }
    }
}

/// ジョブのテストで使う、メモリ上の Misskey。
#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use super::{Emoji, Error, Misskey, NewNote, Note};
    use crate::config::Visibility;
    use crate::mfm::{MAX_NOTE_LENGTH, note_length};

    /// 失敗させたい操作。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Fail {
        Emojis,
        Create,
        /// 投稿を INVALID_PARAM で拒む。
        CreateInvalid,
        /// 投稿は保存したのに失敗を返す（応答の前に通信が切れた場合）。
        CreateStored,
    }

    /// 作ったノート。
    #[derive(Debug, Clone, PartialEq)]
    pub struct Created {
        pub text: String,
        pub visibility: Visibility,
    }

    #[derive(Debug, Default)]
    struct State {
        emojis: Vec<Emoji>,
        /// 自分が作ったノート。作成順。
        created: Vec<Created>,
        next_id: u64,
        fail: Option<Fail>,
        /// 応答を返さずに止まる操作。
        hang: Option<Fail>,
    }

    #[derive(Debug, Default)]
    pub struct FakeMisskey {
        state: Mutex<State>,
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

    impl FakeMisskey {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_emojis(self, emojis: Vec<Emoji>) -> Self {
            self.state.lock().unwrap().emojis = emojis;
            self
        }

        pub fn set_emojis(&self, emojis: Vec<Emoji>) {
            self.state.lock().unwrap().emojis = emojis;
        }

        /// 指定した操作を失敗させる。
        pub fn failing(self, fail: Fail) -> Self {
            self.state.lock().unwrap().fail = Some(fail);
            self
        }

        pub fn set_failing(&self, fail: Option<Fail>) {
            self.state.lock().unwrap().fail = fail;
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
            self.created().into_iter().map(|c| c.text).collect()
        }
    }

    impl State {
        /// `operation` を失敗させるか。
        fn should_fail(&self, operation: Fail) -> bool {
            self.fail == Some(operation)
        }

        fn push(&mut self, text: &str, visibility: Visibility) -> Note {
            let note = Note {
                id: format!("note{}", self.next_id),
            };
            self.next_id += 1;
            self.created.push(Created {
                text: text.to_owned(),
                visibility,
            });
            note
        }
    }

    impl Misskey for FakeMisskey {
        async fn emojis(&self) -> Result<Vec<Emoji>, Error> {
            let state = self.state.lock().unwrap();
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
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn a_note_over_the_length_limit_is_rejected() {
            let misskey = FakeMisskey::new();
            fn note(text: &str) -> NewNote<'_> {
                NewNote {
                    text,
                    visibility: Visibility::Home,
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
    use wiremock::matchers::{body_json, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use chrono::Utc;

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
    async fn connect_verifies_the_token() {
        let (_server, _misskey) = connected().await;
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
    async fn create_note_sends_visibility_and_text() {
        let (server, misskey) = connected().await;
        Mock::given(path("/api/notes/create"))
            .and(body_json(
                json!({"visibility": "specified", "text": "hello", "i": "tok"}),
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
            })
            .await
            .unwrap();
        assert_eq!(note.id, note_json()["id"].as_str().unwrap());
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
        misskey
            .create_note(NewNote {
                text: &text,
                visibility: Visibility::Specified,
            })
            .await
            .unwrap();
    }
}
