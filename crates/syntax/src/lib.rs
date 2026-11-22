#![forbid(unsafe_code)]

mod lexer;

use lexer::{Token, TokenKind};
use ops_spec::{Error, ErrorCode, Path, Segment, Value};

pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 64;

#[derive(Debug)]
pub struct Script {
    pub statements: Vec<Statement>,
}

#[derive(Debug)]
pub struct Statement {
    pub line: usize,
    pub destination: Option<String>,
    pub instruction: Instruction,
}

#[derive(Debug)]
pub enum Instruction {
    Call {
        name: String,
        arguments: Vec<Operand>,
    },
    Where {
        source: Operand,
        predicate: Expression,
    },
    Take {
        source: Operand,
        count: u64,
    },
    Wait {
        source: Operand,
        seconds: u64,
    },
    Assert {
        predicate: Expression,
        text: String,
    },
    Out(Vec<Operand>),
    Unknown(String),
}

#[derive(Clone, Debug)]
pub struct Operand {
    pub kind: OperandKind,
    pub text: String,
    pub column: usize,
}

#[derive(Clone, Debug)]
pub enum OperandKind {
    Literal(Value),
    Reference(Path),
    Target(usize),
}

#[derive(Debug)]
pub enum Expression {
    Value(Operand),
    Not(Box<Expression>),
    And(Vec<Expression>),
    Or(Vec<Expression>),
    Compare(Comparison, Operand, Operand),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    Equal,
    NotEqual,
    Greater,
    Less,
    GreaterEqual,
    LessEqual,
}

pub fn parse(text: &str) -> Result<Script, Error> {
    if text.len() > MAX_SOURCE_BYTES {
        return Err(
            Error::new(ErrorCode::EParse, "Script exceeds the source size limit")
                .at(1)
                .column(1),
        );
    }
    let mut statements = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        let number = index + 1;
        let tokens = lexer::lex(line, number)?;
        if !tokens.is_empty() {
            statements.push(
                LineParser {
                    source: line,
                    tokens,
                    at: 0,
                    line: number,
                }
                .statement()?,
            );
        }
    }
    Ok(Script { statements })
}

struct LineParser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    at: usize,
    line: usize,
}

impl LineParser<'_> {
    fn statement(mut self) -> Result<Statement, Error> {
        let first = self.word("Expected an instruction name or result name")?;
        let (destination, name) = if self.consume(&TokenKind::Assign) {
            if matches!(first.as_str(), "and" | "or" | "not") {
                return Err(self.error("Invalid result name"));
            }
            (Some(first), self.word("Expected an instruction after =")?)
        } else {
            (None, first)
        };

        let instruction = match name.as_str() {
            "call" => self.call()?,
            "where" => {
                self.space()?;
                let source = self.operand()?;
                self.space()?;
                let predicate = self.expression()?;
                Instruction::Where { source, predicate }
            }
            "take" | "wait" => self.bounded(&name)?,
            "assert" => {
                self.space()?;
                let start = self
                    .tokens
                    .get(self.at)
                    .map_or(self.source.len(), |token| token.start);
                let predicate = self.expression()?;
                let text = self.source[start..self.end()].to_owned();
                Instruction::Assert { predicate, text }
            }
            "out" => {
                if self.at == self.tokens.len() {
                    return Err(self.error("out requires at least one value"));
                }
                Instruction::Out(self.arguments()?)
            }
            _ => {
                return Ok(Statement {
                    line: self.line,
                    destination,
                    instruction: Instruction::Unknown(name),
                })
            }
        };
        if self.at < self.tokens.len() {
            return Err(self.error("Unexpected token after instruction"));
        }
        Ok(Statement {
            line: self.line,
            destination,
            instruction,
        })
    }

    fn call(&mut self) -> Result<Instruction, Error> {
        self.space()?;
        let namespace = self.word("Expected an action namespace")?;
        self.adjacent()?;
        self.expect(&TokenKind::Dot, "Expected namespace.action")?;
        self.adjacent()?;
        let action = self.word("Expected an action name")?;
        Ok(Instruction::Call {
            name: format!("{namespace}.{action}"),
            arguments: self.arguments()?,
        })
    }

    fn arguments(&mut self) -> Result<Vec<Operand>, Error> {
        let mut arguments = Vec::new();
        while self.at < self.tokens.len() {
            self.space()?;
            arguments.push(self.operand()?);
        }
        Ok(arguments)
    }

    fn bounded(&mut self, name: &str) -> Result<Instruction, Error> {
        self.space()?;
        let source = self.operand()?;
        self.space()?;
        let count = self.unsigned("Expected a nonnegative integer limit")?;
        if name == "wait" {
            Ok(Instruction::Wait {
                source,
                seconds: count,
            })
        } else {
            Ok(Instruction::Take { source, count })
        }
    }

    fn expression(&mut self) -> Result<Expression, Error> {
        let first = self.conjunction()?;
        if !self.consume_word("or") {
            return Ok(first);
        }
        let mut list = vec![first, self.conjunction()?];
        while self.consume_word("or") {
            list.push(self.conjunction()?);
        }
        Ok(Expression::Or(list))
    }

    fn conjunction(&mut self) -> Result<Expression, Error> {
        let first = self.negation(0)?;
        if !self.consume_word("and") {
            return Ok(first);
        }
        let mut list = vec![first, self.negation(0)?];
        while self.consume_word("and") {
            list.push(self.negation(0)?);
        }
        Ok(Expression::And(list))
    }

    fn negation(&mut self, depth: usize) -> Result<Expression, Error> {
        if self.consume_word("not") {
            if depth >= MAX_DEPTH {
                return Err(self.error("Expression nesting exceeds the limit"));
            }
            return Ok(Expression::Not(Box::new(self.negation(depth + 1)?)));
        }
        let left = self.operand()?;
        let Some(TokenKind::Compare(comparison)) =
            self.tokens.get(self.at).map(|token| &token.kind)
        else {
            return Ok(Expression::Value(left));
        };
        let comparison = *comparison;
        self.at += 1;
        Ok(Expression::Compare(comparison, left, self.operand()?))
    }

    fn operand(&mut self) -> Result<Operand, Error> {
        let token = self
            .tokens
            .get(self.at)
            .ok_or_else(|| self.error("Expected a value or reference"))?;
        let start = token.start;
        let kind = token.kind.clone();
        self.at += 1;
        let kind = match kind {
            TokenKind::Literal(value) => OperandKind::Literal(value),
            TokenKind::Target(index) => OperandKind::Target(index),
            TokenKind::Word(root) if !matches!(root.as_str(), "and" | "or" | "not") => {
                OperandKind::Reference(self.path(root)?)
            }
            _ => {
                return Err(
                    Error::new(ErrorCode::EParse, "Expected a value or reference")
                        .at(self.line)
                        .column(start + 1),
                )
            }
        };
        Ok(Operand {
            kind,
            text: self.source[start..self.end()].to_owned(),
            column: start + 1,
        })
    }

    fn path(&mut self, root: String) -> Result<Path, Error> {
        let mut segments = Vec::new();
        while let Some(token) = self.tokens.get(self.at) {
            if token.start != self.end() {
                break;
            }
            let segment = match token.kind {
                TokenKind::Dot => {
                    self.at += 1;
                    self.adjacent()?;
                    Segment::Field(self.word("Expected a field name after .")?)
                }
                TokenKind::Open => {
                    self.at += 1;
                    self.adjacent()?;
                    let index = usize::try_from(self.unsigned("Expected an array index")?)
                        .map_err(|_| self.error("Array index is too large"))?;
                    self.adjacent()?;
                    self.expect(&TokenKind::Close, "Expected ] after array index")?;
                    Segment::Index(index)
                }
                _ => break,
            };
            segments.push(segment);
            if segments.len() > MAX_DEPTH {
                return Err(self.error("Field path exceeds the nesting limit"));
            }
        }
        Ok(Path { root, segments })
    }

    fn unsigned(&mut self, message: &str) -> Result<u64, Error> {
        let value = self
            .tokens
            .get(self.at)
            .and_then(|token| match &token.kind {
                TokenKind::Literal(value) => value.as_u64(),
                _ => None,
            })
            .ok_or_else(|| self.error(message))?;
        self.at += 1;
        Ok(value)
    }

    fn word(&mut self, message: &str) -> Result<String, Error> {
        let Some(TokenKind::Word(word)) = self.tokens.get(self.at).map(|token| &token.kind) else {
            return Err(self.error(message));
        };
        self.at += 1;
        Ok(word.clone())
    }

    fn consume_word(&mut self, word: &str) -> bool {
        if matches!(self.tokens.get(self.at).map(|token| &token.kind), Some(TokenKind::Word(value)) if value == word)
        {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn consume(&mut self, kind: &TokenKind) -> bool {
        if self
            .tokens
            .get(self.at)
            .is_some_and(|token| token.kind == *kind)
        {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, message: &str) -> Result<(), Error> {
        if self.consume(kind) {
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    fn end(&self) -> usize {
        self.at
            .checked_sub(1)
            .and_then(|previous| self.tokens.get(previous))
            .map_or(0, |token| token.end)
    }

    fn space(&self) -> Result<(), Error> {
        if self
            .tokens
            .get(self.at)
            .is_some_and(|token| token.start == self.end())
        {
            Err(self.error("Expected a space between arguments"))
        } else {
            Ok(())
        }
    }

    fn adjacent(&self) -> Result<(), Error> {
        if self
            .tokens
            .get(self.at)
            .is_some_and(|token| token.start == self.end())
        {
            Ok(())
        } else {
            Err(self.error("Expected a contiguous reference or action name"))
        }
    }

    fn error(&self, message: &str) -> Error {
        let column = self
            .tokens
            .get(self.at)
            .map_or(self.source.len(), |token| token.start)
            + 1;
        Error::new(ErrorCode::EParse, message)
            .at(self.line)
            .column(column)
    }
}
