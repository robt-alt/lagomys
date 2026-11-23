use ops_spec::{Error, ErrorCode, Value};

use crate::Comparison;

const MAX_TOKENS_PER_LINE: usize = 256;

#[derive(Debug)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind {
    Word(String),
    Literal(Value),
    Target(usize),
    Compare(Comparison),
    Assign,
    Dot,
    Open,
    Close,
    OpenBlock,
    CloseBlock,
}

pub(crate) fn lex(line: &str, number: usize) -> Result<Vec<Token>, Error> {
    let mut cursor = 0;
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    while cursor < bytes.len() {
        if bytes[cursor] == b' ' {
            cursor += 1;
            continue;
        }
        let start = cursor;
        let error = |message| {
            Error::new(ErrorCode::EParse, message)
                .at(number)
                .column(start + 1)
        };
        let first = bytes[cursor];
        cursor += 1;
        let kind = match first {
            b'.' => TokenKind::Dot,
            b'[' => TokenKind::Open,
            b']' => TokenKind::Close,
            b'{' => TokenKind::OpenBlock,
            b'}' => TokenKind::CloseBlock,
            b'@' => {
                while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                    cursor += 1;
                }
                let index = line[start + 1..cursor]
                    .parse()
                    .map_err(|_| error("Invalid target reference"))?;
                TokenKind::Target(index)
            }
            b'"' => {
                cursor = string_end(bytes, cursor)
                    .ok_or_else(|| error("Unterminated string literal"))?;
                let value = serde_json::from_str(&line[start..cursor])
                    .map_err(|_| error("Invalid JSON string literal"))?;
                TokenKind::Literal(value)
            }
            b'0'..=b'9' | b'-' => {
                while bytes.get(cursor).is_some_and(|byte| {
                    byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-')
                }) {
                    cursor += 1;
                }
                let value = serde_json::from_str::<serde_json::Number>(&line[start..cursor])
                    .map_err(|_| error("Invalid JSON number literal"))?;
                TokenKind::Literal(Value::Number(value))
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                while bytes
                    .get(cursor)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    cursor += 1;
                }
                match &line[start..cursor] {
                    "true" => TokenKind::Literal(Value::Bool(true)),
                    "false" => TokenKind::Literal(Value::Bool(false)),
                    name => TokenKind::Word(name.to_owned()),
                }
            }
            b'=' | b'!' | b'>' | b'<' => {
                let has_equal = bytes.get(cursor) == Some(&b'=');
                if has_equal {
                    cursor += 1;
                }
                match (first, has_equal) {
                    (b'=', false) => TokenKind::Assign,
                    (b'=', true) => TokenKind::Compare(Comparison::Equal),
                    (b'!', true) => TokenKind::Compare(Comparison::NotEqual),
                    (b'>', false) => TokenKind::Compare(Comparison::Greater),
                    (b'<', false) => TokenKind::Compare(Comparison::Less),
                    (b'>', true) => TokenKind::Compare(Comparison::GreaterEqual),
                    (b'<', true) => TokenKind::Compare(Comparison::LessEqual),
                    _ => return Err(error("Expected a comparison operator")),
                }
            }
            _ => return Err(error("Unexpected character in script")),
        };
        tokens.push(Token {
            kind,
            start,
            end: cursor,
        });
        if tokens.len() > MAX_TOKENS_PER_LINE {
            return Err(error("Instruction exceeds the token limit"));
        }
    }
    Ok(tokens)
}

fn string_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    while let Some(byte) = bytes.get(cursor) {
        cursor += 1;
        match byte {
            b'\\' => cursor += 1,
            b'"' => return Some(cursor),
            _ => {}
        }
    }
    None
}
