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

    use super::{BannerFile, Emoji, Error, Misskey, NewNote, Note};
    use crate::config::Visibility;
    use crate::mfm::{MAX_NOTE_LENGTH, note_length};

    /// 失敗させたい操作。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Fail {
        Emojis,
        Create,
        Upload,
        SetBanner,
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
        files: Vec<File>,
        /// 次に上げるファイルの通し番号。削除しても ID は重ならない。
        next_file: u64,
        /// `upload_file` が呼ばれた回数。
        uploads: usize,
        banner_id: Option<String>,
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
        misskey
            .create_note(NewNote {
                text: &text,
                visibility: Visibility::Specified,
            })
            .await
            .unwrap();

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
    }
}
