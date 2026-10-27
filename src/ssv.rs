//! 空白区切りの欄の読み取り。旧版の `SsvParser` の移植で、ガチャの引数（個数とカテゴリ）に使う。
//!
//! 欄は空白で区切る。`"` か `'` で囲むと空白を含められる（`""` は空の欄）。閉じ引用符の直後に空白以外が続けば
//! 引用符ごと 1 つの欄にし（`"a b"c` → `"a b"c`）、閉じていない引用符は開き引用符込みで返す（`"abc` → `"abc`）。

/// `text` を欄に分ける。
pub fn parse(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut fields = Vec::new();
    let mut state = State::Separated;
    for index in 0..=chars.len() {
        let c = chars.get(index).copied();
        let next = state.next(index, c);
        if let (Some((start, end)), State::Separated) = (state.field(), &next) {
            fields.push(chars[start..=end].iter().collect());
        }
        state = next;
    }
    fields
}

#[derive(Debug, Clone, Copy)]
enum State {
    Separated,
    /// 引用符の無い欄。`start..=end` が欄。
    Field {
        start: usize,
        end: usize,
    },
    /// 開き引用符の後。`start` は引用符の位置、`end` は最後の文字。
    QuoteStart {
        quote: char,
        start: usize,
        end: usize,
    },
    /// 閉じ引用符の後。`start..=end` が引用符の中身。
    QuoteEnd {
        quote: char,
        start: usize,
        end: usize,
    },
}

impl State {
    fn next(self, index: usize, c: Option<char>) -> State {
        let is_separator = c.is_none_or(char::is_whitespace);
        match self {
            State::Separated => match c {
                Some(q @ ('"' | '\'')) => State::QuoteStart {
                    quote: q,
                    start: index,
                    end: index,
                },
                Some(_) if !is_separator => State::Field {
                    start: index,
                    end: index,
                },
                _ => State::Separated,
            },
            State::Field { start, end } => {
                if is_separator {
                    State::Separated
                } else {
                    State::Field {
                        start,
                        end: end + 1,
                    }
                }
            }
            State::QuoteStart { quote, start, end } => match c {
                None => State::Separated,
                Some(c) if c == quote => State::QuoteEnd {
                    quote,
                    start: start + 1,
                    end,
                },
                Some(_) => State::QuoteStart {
                    quote,
                    start,
                    end: end + 1,
                },
            },
            State::QuoteEnd { quote, start, end } => {
                if is_separator {
                    State::Separated
                } else {
                    State::QuoteStart {
                        quote,
                        start: start - 1,
                        end: end + 2,
                    }
                }
            }
        }
    }

    /// 欄の中なら、その範囲。`QuoteEnd` で `end < start` なら空の欄。
    fn field(self) -> Option<(usize, usize)> {
        match self {
            State::Separated => None,
            State::Field { start, end } | State::QuoteStart { start, end, .. } => {
                Some((start, end))
            }
            State::QuoteEnd { start, end, .. } => Some((start, end)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(parse("ガチャ 3 顔"), ["ガチャ", "3", "顔"]);
        assert_eq!(parse("  a\tb\n c  "), ["a", "b", "c"]);
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
    }

    #[test]
    fn quotes_keep_spaces_and_an_empty_quote_is_an_empty_field() {
        assert_eq!(parse(r#"a "b c" 'd e' f"#), ["a", "b c", "d e", "f"]);
        assert_eq!(parse(r#"x "" y"#), ["x", "", "y"]);
        assert_eq!(parse(r#""""#), [""]);
    }

    #[test]
    fn a_quote_followed_by_text_and_an_unclosed_quote_keep_the_quotes() {
        assert_eq!(parse(r#""a b"c"#), [r#""a b"c"#]);
        assert_eq!(parse(r#""abc"#), [r#""abc"#]);
        assert_eq!(parse(r#"'x"#), [r#"'x"#]);
    }
}
