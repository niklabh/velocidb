//! SQL tokenizer.
//!
//! Turns SQL text into a flat list of [`Token`]s, each carrying its byte span
//! in the source. The parser uses spans to slice out the original text of
//! expressions it still passes to the executor as strings (select-list items,
//! `ORDER BY` expressions, vector constructors).

use crate::types::{Result, VelociError};

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// Bare identifier or keyword; keywords are matched case-insensitively
    /// by the parser, so the lexer does not distinguish them.
    Word(String),
    /// Numeric literal as written (sign excluded), e.g. `42`, `3.5`, `1e3`.
    Number(String),
    /// `'...'` or `"..."`, unescaped. `quote` tells the parser which form was
    /// used: a double-quoted token is an identifier where one is expected.
    String {
        value: String,
        quote: char,
    },
    /// `` `...` `` quoted identifier.
    QuotedIdent(String),
    /// `X'...'` blob literal; the hex digits are validated by the parser.
    Blob(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Star,
    Plus,
    Minus,
    Dot,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset one past the last character.
    pub end: usize,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    let mut lexer = Lexer {
        sql,
        chars: sql.char_indices().peekable(),
        tokens: Vec::new(),
    };
    lexer.run()?;
    Ok(lexer.tokens)
}

struct Lexer<'a> {
    sql: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    tokens: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn pos(&mut self) -> usize {
        self.chars.peek().map(|&(i, _)| i).unwrap_or(self.sql.len())
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().map(|&(_, c)| c)
    }

    /// The character after the next one.
    fn peek2(&self) -> Option<char> {
        let mut it = self.chars.clone();
        it.next();
        it.next().map(|(_, c)| c)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        let end = self.pos();
        self.tokens.push(Token { kind, start, end });
    }

    fn run(&mut self) -> Result<()> {
        while let Some(c) = self.peek() {
            let start = self.pos();

            if c.is_whitespace() {
                self.chars.next();
                continue;
            }

            // `-- comment` to end of line.
            if c == '-' && self.peek2() == Some('-') {
                while let Some(c) = self.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.chars.next();
                }
                continue;
            }

            // X'...' blob literal (must be checked before identifiers).
            if (c == 'x' || c == 'X') && self.peek2() == Some('\'') {
                self.chars.next();
                self.chars.next();
                let mut hex = String::new();
                loop {
                    match self.chars.next() {
                        Some((_, '\'')) => break,
                        Some((_, c)) => hex.push(c),
                        None => return Err(error_at(start, "Unterminated BLOB literal")),
                    }
                }
                self.push(TokenKind::Blob(hex), start);
                continue;
            }

            if is_ident_start(c) {
                let mut word = String::new();
                while let Some(c) = self.peek() {
                    if !is_ident_char(c) {
                        break;
                    }
                    word.push(c);
                    self.chars.next();
                }
                self.push(TokenKind::Word(word), start);
                continue;
            }

            if c.is_ascii_digit() || (c == '.' && self.peek2().is_some_and(|d| d.is_ascii_digit()))
            {
                let number = self.number();
                self.push(TokenKind::Number(number), start);
                continue;
            }

            match c {
                '\'' | '"' => {
                    let value = self.quoted(c, start)?;
                    self.push(TokenKind::String { value, quote: c }, start);
                }
                '`' => {
                    let value = self.quoted(c, start)?;
                    self.push(TokenKind::QuotedIdent(value), start);
                }
                _ => {
                    self.chars.next();
                    let next = self.peek();
                    let kind = match (c, next) {
                        ('<', Some('=')) => Some(TokenKind::LtEq),
                        ('<', Some('>')) => Some(TokenKind::NotEq),
                        ('>', Some('=')) => Some(TokenKind::GtEq),
                        ('!', Some('=')) => Some(TokenKind::NotEq),
                        ('=', Some('=')) => Some(TokenKind::Eq),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        self.chars.next();
                        self.push(kind, start);
                        continue;
                    }
                    let kind = match c {
                        '(' => TokenKind::LParen,
                        ')' => TokenKind::RParen,
                        '[' => TokenKind::LBracket,
                        ']' => TokenKind::RBracket,
                        ',' => TokenKind::Comma,
                        ';' => TokenKind::Semicolon,
                        '*' => TokenKind::Star,
                        '+' => TokenKind::Plus,
                        '-' => TokenKind::Minus,
                        '.' => TokenKind::Dot,
                        '=' => TokenKind::Eq,
                        '<' => TokenKind::Lt,
                        '>' => TokenKind::Gt,
                        _ => return Err(error_at(start, &format!("Unexpected character '{}'", c))),
                    };
                    self.push(kind, start);
                }
            }
        }
        Ok(())
    }

    /// Digits, optional fraction, optional exponent.
    fn number(&mut self) -> String {
        let mut out = String::new();
        let digits = |lexer: &mut Self, out: &mut String| {
            while let Some(c) = lexer.peek() {
                if !c.is_ascii_digit() {
                    break;
                }
                out.push(c);
                lexer.chars.next();
            }
        };

        digits(self, &mut out);
        if self.peek() == Some('.') {
            out.push('.');
            self.chars.next();
            digits(self, &mut out);
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let sign = self.peek2();
            let has_exponent = match sign {
                Some(d) if d.is_ascii_digit() => true,
                Some('+' | '-') => {
                    let mut it = self.chars.clone();
                    it.next();
                    it.next();
                    it.next().is_some_and(|(_, d)| d.is_ascii_digit())
                }
                _ => false,
            };
            if has_exponent {
                out.push(self.chars.next().unwrap().1);
                if matches!(self.peek(), Some('+' | '-')) {
                    out.push(self.chars.next().unwrap().1);
                }
                digits(self, &mut out);
            }
        }
        out
    }

    /// Reads a quoted run starting at the opening `quote`. A doubled quote
    /// is a literal quote (standard SQL); `\<quote>` and `\\` are accepted
    /// too for compatibility with the earlier parser. Any other backslash is
    /// kept as-is.
    fn quoted(&mut self, quote: char, start: usize) -> Result<String> {
        self.chars.next(); // opening quote
        let mut out = String::new();
        loop {
            match self.chars.next() {
                None => {
                    let what = if quote == '`' { "identifier" } else { "string" };
                    return Err(error_at(start, &format!("Unterminated quoted {}", what)));
                }
                Some((_, '\\')) if matches!(self.peek(), Some(c) if c == quote || c == '\\') => {
                    out.push(self.chars.next().unwrap().1);
                }
                Some((_, c)) if c == quote => {
                    if self.peek() == Some(quote) {
                        self.chars.next();
                        out.push(quote);
                    } else {
                        return Ok(out);
                    }
                }
                Some((_, c)) => out.push(c),
            }
        }
    }
}

fn error_at(offset: usize, msg: &str) -> VelociError {
    VelociError::ParseError(format!("{} at position {}", msg, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        tokenize(sql).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn test_tokenize_operators_and_punctuation() {
        use TokenKind::*;
        assert_eq!(
            kinds("a>=1,b<>2;(c!=3)<=[*]"),
            vec![
                Word("a".into()),
                GtEq,
                Number("1".into()),
                Comma,
                Word("b".into()),
                NotEq,
                Number("2".into()),
                Semicolon,
                LParen,
                Word("c".into()),
                NotEq,
                Number("3".into()),
                RParen,
                LtEq,
                LBracket,
                Star,
                RBracket,
            ]
        );
    }

    #[test]
    fn test_tokenize_strings_and_escapes() {
        use TokenKind::*;
        assert_eq!(
            kinds(r#"'it''s' 'a\'b' "dq" `bq` 'C:\dir'"#),
            vec![
                String {
                    value: "it's".into(),
                    quote: '\''
                },
                String {
                    value: "a'b".into(),
                    quote: '\''
                },
                String {
                    value: "dq".into(),
                    quote: '"'
                },
                QuotedIdent("bq".into()),
                String {
                    value: r"C:\dir".into(),
                    quote: '\''
                },
            ]
        );
        assert!(tokenize("'open").is_err());
    }

    #[test]
    fn test_tokenize_numbers_blobs_comments() {
        use TokenKind::*;
        assert_eq!(
            kinds("1 2.5 .5 1e3 2E-2 3e x'0a' -- trailing\n7"),
            vec![
                Number("1".into()),
                Number("2.5".into()),
                Number(".5".into()),
                Number("1e3".into()),
                Number("2E-2".into()),
                Number("3".into()),
                Word("e".into()),
                Blob("0a".into()),
                Number("7".into()),
            ]
        );
    }

    #[test]
    fn test_token_spans() {
        let sql = "SELECT  f(x) FROM t";
        let tokens = tokenize(sql).unwrap();
        assert_eq!(&sql[tokens[1].start..tokens[4].end], "f(x)");
    }
}
