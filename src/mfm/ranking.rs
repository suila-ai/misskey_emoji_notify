//! 前日のリアクションのランキングの本文。旧版の `DailyJobRunner.AggregateRanking` と同じ形。

use std::collections::HashMap;

use chrono::NaiveDate;

use super::{MAX_NOTE_LENGTH, note_length};

/// 載せる順位の数。同数で並んだ順位がこれに掛かるグループまで載せる。
const TOP: usize = 10;
const RANK_MARKS: [&str; TOP] = ["🥇", "🥈", "🥉", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣", "🔟"];

/// 集計の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranking {
    pub date: NaiveDate,
    pub notes: usize,
    pub reactions: u64,
    /// 同数のグループ。順位の順。
    pub groups: Vec<Group>,
}

/// 同じ数のリアクション。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// 0 始まりの順位（グループの先頭）。
    pub rank: usize,
    pub count: u64,
    /// 絵文字の表記（`:name:` か Unicode）。名前順。
    pub emojis: Vec<String>,
}

/// リアクションのキーが集計の対象か。ローカルのカスタム絵文字（`:name@.:`）と Unicode の絵文字。リモートのものは除く。
/// 旧版の正規表現 `^(:.+@\.:)|([^:]+)$` と同じ。
fn is_local_or_unicode(key: &str) -> bool {
    (key.starts_with(':') && key.ends_with("@.:")) || !key.ends_with(':')
}

/// `notes` のリアクションを集計する。`reactions` は（キー、数）の列。
pub fn aggregate<'a>(
    date: NaiveDate,
    notes: usize,
    reactions: impl IntoIterator<Item = (&'a str, u64)>,
) -> Ranking {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for (key, count) in reactions {
        if !is_local_or_unicode(key) {
            continue;
        }
        *counts.entry(key.replace("@.", "")).or_default() += count;
    }
    let total = counts.values().sum();
    let mut sorted: Vec<(String, u64)> = counts.into_iter().collect();
    sorted.sort_by(|(a_name, a), (b_name, b)| b.cmp(a).then_with(|| a_name.cmp(b_name)));

    let mut groups: Vec<Group> = Vec::new();
    for (index, (emoji, count)) in sorted.into_iter().enumerate() {
        match groups.last_mut() {
            Some(group) if group.count == count => group.emojis.push(emoji),
            _ => {
                if index >= TOP {
                    break;
                }
                groups.push(Group {
                    rank: index,
                    count,
                    emojis: vec![emoji],
                });
            }
        }
    }
    Ranking {
        date,
        notes,
        reactions: total,
        groups,
    }
}

/// 見出し。これで自分のノートから読み戻す。
fn header(date: NaiveDate) -> String {
    format!("【昨日({})のリアクション】", date.format("%m/%d"))
}

/// `text` が `date` のランキングか。
pub fn is_for(text: &str, date: NaiveDate) -> bool {
    text.lines().next() == Some(header(date).as_str())
}

/// 本文。1 位、2〜3 位、4 位以下の 3 行に分け、各行はグループを空白 2 つでつなぐ。
///
/// ノートの上限を超えるときは、収まる数まで後ろから絵文字を除き、途中で切ったグループの末尾に `…` を付ける。
/// 見出しで投稿済みかを読み戻すので、ノートは分けない。
pub fn render(ranking: &Ranking) -> String {
    let total: usize = ranking.groups.iter().map(|group| group.emojis.len()).sum();
    let text = render_first(ranking, total);
    if note_length(&text) <= MAX_NOTE_LENGTH {
        return text;
    }
    // 長さは載せる数について減らないので、収まる最大の数を二分探索する。
    let (mut fits, mut over) = (0, total);
    while over - fits > 1 {
        let middle = (fits + over) / 2;
        if note_length(&render_first(ranking, middle)) <= MAX_NOTE_LENGTH {
            fits = middle;
        } else {
            over = middle;
        }
    }
    render_first(ranking, fits)
}

/// 先頭から `count` 個の絵文字だけを載せた本文。
fn render_first(ranking: &Ranking, count: usize) -> String {
    let mut lines: Vec<Vec<String>> = vec![Vec::new(), Vec::new(), Vec::new()];
    let mut left = count;
    for group in &ranking.groups {
        if left == 0 {
            break;
        }
        let shown = group.emojis.len().min(left);
        left -= shown;
        let mut emojis = group.emojis[..shown].concat();
        if shown < group.emojis.len() {
            emojis.push('…');
        }
        let line = match group.rank {
            0 => 0,
            1 | 2 => 1,
            _ => 2,
        };
        lines[line].push(format_group(group, &emojis));
    }
    let body: Vec<String> = lines
        .into_iter()
        .filter(|line| !line.is_empty())
        .map(|line| line.join("  "))
        .collect();
    format!(
        "{}\n{}\n\nノート数: {} リアクション数: {}",
        header(ranking.date),
        body.join("\n"),
        ranking.notes,
        ranking.reactions
    )
}

/// グループ 1 つ。`emojis` は載せる絵文字をつないだもの。
fn format_group(group: &Group, emojis: &str) -> String {
    let text = format!("{} {emojis}", RANK_MARKS[group.rank]);
    if group.rank == 0 {
        format!("$[x2 {text}] x{}", group.count)
    } else {
        format!("{text} x{}", group.count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 22).unwrap()
    }

    #[test]
    fn remote_reactions_are_excluded_and_local_names_lose_the_host() {
        let ranking = aggregate(
            date(),
            3,
            [
                (":a@.:", 2),
                (":a@.:", 3),
                ("👍", 1),
                (":remote@example.com:", 9),
                (":b@.:", 5),
            ],
        );
        assert_eq!(ranking.reactions, 11);
        assert_eq!(
            ranking.groups,
            [
                Group {
                    rank: 0,
                    count: 5,
                    emojis: vec![":a:".into(), ":b:".into()]
                },
                Group {
                    rank: 2,
                    count: 1,
                    emojis: vec!["👍".into()]
                },
            ]
        );
    }

    #[test]
    fn ties_share_a_rank_and_the_list_stops_after_the_group_at_the_tenth_rank() {
        // 10 位（rank 9）に掛かる同数のグループまで載せ、11 位からは載せない。
        let mut reactions: Vec<(String, u64)> =
            (0..9).map(|i| (format!(":e{i}@.:"), 100 - i)).collect();
        reactions.push((":tie1@.:".to_owned(), 1));
        reactions.push((":tie2@.:".to_owned(), 1));
        reactions.push((":tie3@.:".to_owned(), 1));
        let ranking = aggregate(date(), 1, reactions.iter().map(|(k, v)| (k.as_str(), *v)));
        assert_eq!(ranking.groups.len(), 10);
        let last = ranking.groups.last().unwrap();
        assert_eq!(last.rank, 9);
        assert_eq!(last.emojis, [":tie1:", ":tie2:", ":tie3:"]);

        // 11 位以下だけの同数グループは載せない。
        let mut reactions: Vec<(String, u64)> =
            (0..10).map(|i| (format!(":e{i}@.:"), 100 - i)).collect();
        reactions.push((":x@.:".to_owned(), 1));
        let ranking = aggregate(date(), 1, reactions.iter().map(|(k, v)| (k.as_str(), *v)));
        assert_eq!(ranking.groups.len(), 10);
        assert_eq!(ranking.groups.last().unwrap().emojis, [":e9:"]);
    }

    #[test]
    fn render_matches_the_old_layout() {
        let ranking = Ranking {
            date: date(),
            notes: 123,
            reactions: 45,
            groups: vec![
                Group {
                    rank: 0,
                    count: 10,
                    emojis: vec![":a:".into()],
                },
                Group {
                    rank: 1,
                    count: 8,
                    emojis: vec![":b:".into(), "👍".into()],
                },
                Group {
                    rank: 3,
                    count: 2,
                    emojis: vec![":c:".into()],
                },
                Group {
                    rank: 4,
                    count: 1,
                    emojis: vec![":d:".into()],
                },
            ],
        };
        assert_eq!(
            render(&ranking),
            "【昨日(10/22)のリアクション】\n\
             $[x2 🥇 :a:] x10\n\
             🥈 :b:👍 x8\n\
             4️⃣ :c: x2  5️⃣ :d: x1\n\
             \n\
             ノート数: 123 リアクション数: 45"
        );
    }

    #[test]
    fn empty_lines_are_skipped() {
        let ranking = Ranking {
            date: date(),
            notes: 1,
            reactions: 3,
            groups: vec![Group {
                rank: 0,
                count: 3,
                emojis: vec![":a:".into()],
            }],
        };
        assert_eq!(
            render(&ranking),
            "【昨日(10/22)のリアクション】\n$[x2 🥇 :a:] x3\n\nノート数: 1 リアクション数: 3"
        );
    }

    #[test]
    fn is_for_reads_the_header() {
        let text = render(&aggregate(date(), 1, [(":a@.:", 1)]));
        assert!(is_for(&text, date()));
        assert!(!is_for(&text, date().succ_opt().unwrap()));
        assert!(!is_for("【絵文字追加】\n", date()));
    }

    #[test]
    fn a_long_ranking_is_cut_with_an_ellipsis_and_keeps_the_header_and_totals() {
        // リアクションの少ない日は、x1 の同数グループに多くの絵文字が並ぶ。
        let names: Vec<String> = (0..300).map(|i| format!(":emoji_{i:03}@.:")).collect();
        let reactions =
            std::iter::once((":top@.:", 5)).chain(names.iter().map(|name| (name.as_str(), 1)));
        let text = render(&aggregate(date(), 40, reactions));
        assert!(
            note_length(&text) <= MAX_NOTE_LENGTH,
            "{}",
            note_length(&text)
        );
        assert!(is_for(&text, date()));
        assert!(text.contains("$[x2 🥇 :top:] x5"));
        assert!(text.contains(":emoji_000:"));
        assert!(text.contains("… x1"), "{text}");
        assert!(text.ends_with("\n\nノート数: 40 リアクション数: 305"));
    }
}
