//! 状態ファイル。JSON を原子的に書く。
//!
//! 書くときは同じディレクトリの一時ファイルに書いてから rename するので、途中で落ちても（電源断でも）元のファイルは壊れない。

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read the state file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse the state file {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write the state file {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to create the state directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// 読む。ファイルが無ければ `None`。
pub fn load<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| Error::Parse {
            path: path.to_owned(),
            source,
        })
}

/// 書く。同じディレクトリの一時ファイルに書いて fsync してから rename し、ディレクトリも fsync する。
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let write = |source| Error::Write {
        path: path.to_owned(),
        source,
    };
    let text = serde_json::to_string_pretty(value).expect("the state is serialisable");
    let temp = path.with_added_extension("tmp");
    let mut file = File::create(&temp).map_err(write)?;
    file.write_all(text.as_bytes()).map_err(write)?;
    file.sync_all().map_err(write)?;
    std::fs::rename(&temp, path).map_err(write)?;
    File::open(parent_dir(path))
        .and_then(|dir| dir.sync_all())
        .map_err(write)
}

/// `path` のあるディレクトリ。`emojis.json` のように親の無い相対パスなら、作業ディレクトリ。
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// ディレクトリを作る。あれば何もしない。
pub fn create_dir(path: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(path).map_err(|source| Error::CreateDir {
        path: path.to_owned(),
        source,
    })
}

/// テストで使う、消えるディレクトリ。
#[cfg(test)]
pub mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// `drop` で中身ごと消える、一時ディレクトリの中の一意なディレクトリ。
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            let name = format!(
                "misskey_emoji_notify-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::test_support::TempDir;
    use super::*;

    fn value() -> BTreeMap<String, u32> {
        BTreeMap::from([("a".to_owned(), 1), ("b".to_owned(), 2)])
    }

    #[test]
    fn a_missing_file_is_none() {
        let dir = TempDir::new();
        let loaded: Option<BTreeMap<String, u32>> = load(&dir.join("x.json")).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new();
        let path = dir.join("x.json");
        save(&path, &value()).unwrap();
        assert_eq!(load::<BTreeMap<String, u32>>(&path).unwrap(), Some(value()));

        // 上書きしても一時ファイルは残らない。
        save(&path, &BTreeMap::<String, u32>::new()).unwrap();
        assert_eq!(
            load::<BTreeMap<String, u32>>(&path).unwrap(),
            Some(BTreeMap::new())
        );
        assert!(!path.with_added_extension("tmp").exists());
    }

    #[test]
    fn a_file_without_a_parent_is_in_the_working_directory() {
        assert_eq!(parent_dir(Path::new("x.json")), Path::new("."));
        assert_eq!(parent_dir(Path::new("state/x.json")), Path::new("state"));
    }

    #[test]
    fn a_broken_file_is_a_parse_error() {
        let dir = TempDir::new();
        let path = dir.join("x.json");
        std::fs::write(&path, "{").unwrap();
        let error = load::<BTreeMap<String, u32>>(&path).unwrap_err();
        assert!(matches!(error, Error::Parse { .. }), "{error}");
    }
}
