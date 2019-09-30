use std::convert::TryFrom;
use std::fmt;

mod header;
pub use header::Header;
mod key;
pub use key::Key;
mod inlined;
use crate::parser::{flags, Type};
mod operator;
pub use inlined::Inlined;
pub use operator::Operator;

#[derive(Clone)]
pub struct Token {
    pub source_index: usize,
    flags: flags::Flag,
    pub inner: RawToken,
}

impl Token {
    pub fn new(inner: RawToken, source_index: usize) -> Self {
        Self {
            source_index,
            flags: flags::Flag::default(),
            inner,
        }
    }

    pub fn with_source_index(mut self, source_index: usize) -> Self {
        self.source_index = source_index;
        self
    }

    pub fn untracked(t: RawToken) -> Self {
        Self {
            source_index: 0,
            flags: flags::Flag::default(),
            inner: t,
        }
    }

    pub fn group(vec: Vec<Token>, source_index: usize) -> Token {
        Token {
            source_index,
            flags: flags::Flag::default(),
            inner: RawToken::Group(vec),
        }
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {:#?}", self.source_index, self.inner)
    }
}

impl TryFrom<&[u8]> for Token {
    type Error = ();

    fn try_from(bytes: &[u8]) -> Result<Token, Self::Error> {
        if bytes.is_empty() {
            return Err(());
        }
        let find_inner = || {
            if let Ok(t) = Header::try_from(bytes) {
                return RawToken::Header(t);
            }
            if let Ok(t) = Key::try_from(bytes) {
                return RawToken::Key(t);
            }
            if let Ok(t) = Inlined::try_from(bytes) {
                return RawToken::Inlined(t);
            }
            if let Ok(t) = Operator::try_from(bytes) {
                return RawToken::Operator(t);
            }
            if bytes == b"\n" {
                return RawToken::NewLine;
            }

            RawToken::Identifier(String::from_utf8(bytes.to_vec()).unwrap())
        };

        let t = Token {
            inner: find_inner(),
            source_index: 0,
            flags: flags::Flag::default(),
        };
        Ok(t)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum RawToken {
    Identifier(String),

    Header(Header),
    Key(Key),
    Inlined(Inlined),
    Group(Vec<Token>),
    Operation(Box<(Token, Token)>, Operator),
    Parameterized(String, Vec<Token>),

    Parameter(usize, Type),
    Operator(Operator),

    NewLine,
}

impl PartialEq for Token {
    fn eq(&self, other: &Token) -> bool {
        self.inner.eq(&other.inner)
    }
}

pub fn is_valid_identifier(ident: &str) -> bool {
    const ALLOWED_CHARACTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz_";
    for c in ident.bytes() {
        if !ALLOWED_CHARACTERS.contains(&c) {
            return false;
        }
    }
    true
}
