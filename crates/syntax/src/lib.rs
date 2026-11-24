#![forbid(unsafe_code)]

mod lexer;

use std::collections::BTreeMap;

use lexer::{Token, TokenKind};
use ops_spec::{is_name, Error, ErrorCode, Path, Segment, Value};

pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 64;

#[derive(Debug)]
pub struct Script {
    pub statements: Vec<Statement>,
    pub macro_expansions: Vec<MacroExpansion>,
    pub definitions: BTreeMap<String, usize>,
}

#[derive(Debug)]
pub struct MacroExpansion {
    pub line: usize,
    pub items_name: String,
    pub capture_name: String,
    pub capture_assigned: bool,
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
        arguments: Vec<CallArgument>,
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
    Repeat {
        count: Operand,
        context: Option<String>,
        body: Vec<Statement>,
    },
    If {
        predicate: Expression,
        then_body: Vec<Statement>,
        else_body: Vec<Statement>,
    },
    Break,
    Continue,
    Return(Vec<Operand>),
    Delay(Operand),
    Collect(Vec<Operand>),
    Evidence(Vec<String>),
    Unknown(String),
}

#[derive(Debug)]
pub enum CallArgument {
    Positional(Operand),
    Named { name: String, operand: Operand },
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
    let mut lines = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        let number = index + 1;
        let tokens = lexer::lex(line, number)?;
        if !tokens.is_empty() {
            lines.push(Line {
                source: line,
                tokens,
                number,
            });
        }
    }
    let mut parser = ProgramParser {
        lines: lines.into_iter().peekable(),
        macro_expansions: Vec::new(),
        definitions: BTreeMap::new(),
    };
    let mut statements = Vec::new();
    while let Some(line) = parser.lines.next() {
        line.parser()
            .statement(&mut parser, 0, StatementKind::Plain, &mut statements)?;
    }
    Ok(Script {
        statements,
        macro_expansions: parser.macro_expansions,
        definitions: parser.definitions,
    })
}

struct Line<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    number: usize,
}

impl Line<'_> {
    fn parser(&self) -> LineParser<'_> {
        LineParser {
            source: self.source,
            tokens: &self.tokens,
            at: 0,
            line: self.number,
        }
    }
}

type Lines<'a> = std::iter::Peekable<std::vec::IntoIter<Line<'a>>>;

struct ProgramParser<'a> {
    lines: Lines<'a>,
    macro_expansions: Vec<MacroExpansion>,
    definitions: BTreeMap<String, usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatementKind {
    Plain,
    Inline,
    MacroBody,
}

#[derive(PartialEq, Eq)]
enum BlockKind {
    Plain,
    IfThen,
    ElseAfterInlineThen,
    MacroBody,
}

struct ParsedBlock {
    body: Vec<Statement>,
    continued_else: Option<Vec<Statement>>,
}

struct LineParser<'a> {
    source: &'a str,
    tokens: &'a [Token],
    at: usize,
    line: usize,
}

impl LineParser<'_> {
    fn statement(
        mut self,
        parser: &mut ProgramParser<'_>,
        depth: usize,
        kind: StatementKind,
        statements: &mut Vec<Statement>,
    ) -> Result<(), Error> {
        if kind == StatementKind::MacroBody {
            if let Some(brace) = self.tokens.iter().position(|token| {
                matches!(token.kind, TokenKind::OpenBlock | TokenKind::CloseBlock)
            }) {
                self.at = brace;
                return Err(self.error("A sampling macro body cannot contain a block"));
            }
        }
        let first = self.word("Expected an instruction name or result name")?;
        let (destination, name) = if self.consume(&TokenKind::Assign) {
            if reserved_name(&first) {
                return Err(self.error("Invalid result name"));
            }
            (Some(first), self.word("Expected an instruction after =")?)
        } else {
            (None, first)
        };
        if kind == StatementKind::MacroBody {
            if matches!(
                name.as_str(),
                "break" | "continue" | "return" | "repeat" | "if"
            ) {
                return Err(self.error("Control flow is forbidden in a sampling macro body"));
            }
            if destination.is_none() {
                return Err(self.error("A sampling macro body requires assignment instructions"));
            }
        }
        if let Some(name) = &destination {
            parser.definitions.entry(name.clone()).or_insert(self.line);
        }

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
            "out" | "return" => {
                if self.at == self.tokens.len() {
                    return Err(self.error(&format!("{name} requires at least one value")));
                }
                let values = self.arguments()?;
                if name == "return" {
                    Instruction::Return(values)
                } else {
                    Instruction::Out(values)
                }
            }
            "repeat" | "if" if kind == StatementKind::Inline => {
                return Err(self.error("A single-line block cannot contain another block"));
            }
            "repeat" => {
                self.space()?;
                let count = self.operand()?;
                if matches!(
                    self.tokens.get(self.at).map(|token| &token.kind),
                    Some(TokenKind::Literal(Value::Number(_)))
                ) {
                    return self.sampling_macro(parser, depth, count, destination, statements);
                }
                let context = if matches!(
                    self.tokens.get(self.at).map(|token| &token.kind),
                    Some(TokenKind::Word(_))
                ) {
                    self.space()?;
                    let name = self.word("Expected a loop context name")?;
                    if reserved_name(&name) {
                        return Err(self.error("Invalid loop context name"));
                    }
                    parser.definitions.entry(name.clone()).or_insert(self.line);
                    Some(name)
                } else {
                    None
                };
                let body = self.block(parser, depth, BlockKind::Plain)?.body;
                Instruction::Repeat {
                    count,
                    context,
                    body,
                }
            }
            "if" => {
                self.space()?;
                let predicate = self.expression()?;
                let then_block = self.block(parser, depth, BlockKind::IfThen)?;
                let else_body = if let Some(body) = then_block.continued_else {
                    body
                } else if self.consume_word("else") {
                    self.block(parser, depth, BlockKind::ElseAfterInlineThen)?
                        .body
                } else if let Some(line) = parser
                    .lines
                    .next_if(|line| line.parser().consume_word("else"))
                {
                    let mut else_parser = line.parser();
                    else_parser.at = 1;
                    let body = else_parser.block(parser, depth, BlockKind::Plain)?.body;
                    else_parser.finish()?;
                    body
                } else {
                    Vec::new()
                };
                Instruction::If {
                    predicate,
                    then_body: then_block.body,
                    else_body,
                }
            }
            "else" => return Err(self.error("else requires a preceding if block")),
            "break" => Instruction::Break,
            "continue" => Instruction::Continue,
            "delay" => {
                self.space()?;
                Instruction::Delay(self.operand()?)
            }
            "collect" => Instruction::Collect(self.arguments()?),
            "evidence" => {
                let mut names = Vec::new();
                while self.at < self.tokens.len() {
                    self.space()?;
                    names.push(self.word("Expected a result name")?);
                }
                Instruction::Evidence(names)
            }
            _ => {
                statements.push(Statement {
                    line: self.line,
                    destination,
                    instruction: Instruction::Unknown(name),
                });
                return Ok(());
            }
        };
        self.finish()?;
        statements.push(Statement {
            line: self.line,
            destination,
            instruction,
        });
        Ok(())
    }

    fn sampling_macro(
        &mut self,
        parser: &mut ProgramParser<'_>,
        depth: usize,
        count: Operand,
        destination: Option<String>,
        statements: &mut Vec<Statement>,
    ) -> Result<(), Error> {
        let name =
            destination.ok_or_else(|| self.error("A sampling macro requires a result name"))?;
        if !matches!(
            &count.kind,
            OperandKind::Literal(Value::Number(number)) if number.is_i64() || number.is_u64()
        ) {
            return Err(Error::new(
                ErrorCode::EParse,
                "A sampling macro requires an integer count",
            )
            .at(self.line)
            .column(count.column));
        }
        self.space()?;
        let gap_column = self
            .tokens
            .get(self.at)
            .map_or(self.source.len(), |token| token.start)
            + 1;
        let gap = self.unsigned("A sampling macro requires a nonnegative integer delay")?;
        self.space()?;
        let capture_column = self
            .tokens
            .get(self.at)
            .map_or(self.source.len(), |token| token.start)
            + 1;
        let capture = self.word("Expected a sampling macro capture name")?;
        if reserved_name(&capture) {
            return Err(self.error("Invalid sampling macro capture name"));
        }
        let until = if self.consume_word("until") {
            self.space()?;
            Some(self.expression()?)
        } else {
            None
        };
        // The expanded repeat contains an additional level for its generated if blocks.
        if depth + 1 >= MAX_DEPTH {
            return Err(self.error("Expanded sampling macro exceeds the block nesting limit"));
        }
        let mut body = self.block(parser, depth, BlockKind::MacroBody)?.body;
        self.finish()?;

        let items_name = format!("{name}_items");
        let capture_assigned = body
            .iter()
            .any(|statement| statement.destination.as_deref() == Some(capture.as_str()));
        let generated = |instruction| Statement {
            line: self.line,
            destination: None,
            instruction,
        };
        body.push(generated(Instruction::Collect(vec![
            reference_operand(items_name.clone(), capture_column),
            reference_operand(capture.clone(), capture_column),
        ])));
        if let Some(predicate) = until {
            body.push(generated(Instruction::If {
                predicate,
                then_body: vec![generated(Instruction::Break)],
                else_body: Vec::new(),
            }));
        }
        let last = Operand {
            kind: OperandKind::Reference(Path {
                root: "i".to_owned(),
                segments: vec![Segment::Field("last".to_owned())],
            }),
            text: "i.last".to_owned(),
            column: gap_column,
        };
        let gap = Operand {
            kind: OperandKind::Literal(Value::from(gap)),
            text: gap.to_string(),
            column: gap_column,
        };
        body.push(generated(Instruction::If {
            predicate: Expression::Not(Box::new(Expression::Value(last))),
            then_body: vec![generated(Instruction::Delay(gap))],
            else_body: Vec::new(),
        }));

        statements.push(Statement {
            line: self.line,
            destination: Some(items_name.clone()),
            instruction: Instruction::Collect(Vec::new()),
        });
        statements.push(Statement {
            line: self.line,
            destination: Some(name),
            instruction: Instruction::Repeat {
                count,
                context: Some("i".to_owned()),
                body,
            },
        });
        parser.macro_expansions.push(MacroExpansion {
            line: self.line,
            items_name,
            capture_name: capture,
            capture_assigned,
        });
        Ok(())
    }

    fn block(
        &mut self,
        parser: &mut ProgramParser<'_>,
        depth: usize,
        kind: BlockKind,
    ) -> Result<ParsedBlock, Error> {
        self.expect(&TokenKind::OpenBlock, "Expected { to start a block")?;
        if depth >= MAX_DEPTH {
            return Err(self.error("Block nesting exceeds the limit"));
        }
        let mut body = Vec::new();
        if self.at < self.tokens.len() {
            let start = self.at;
            while let Some(token) = self.tokens.get(self.at) {
                match token.kind {
                    TokenKind::CloseBlock => {
                        let statement_kind = if kind == BlockKind::MacroBody {
                            StatementKind::MacroBody
                        } else {
                            StatementKind::Inline
                        };
                        LineParser {
                            tokens: &self.tokens[start..self.at],
                            at: 0,
                            ..*self
                        }
                        .statement(
                            parser,
                            depth + 1,
                            statement_kind,
                            &mut body,
                        )?;
                        self.at += 1;
                        return Ok(ParsedBlock {
                            body,
                            continued_else: None,
                        });
                    }
                    TokenKind::OpenBlock => {
                        return Err(self.error("A single-line block cannot contain another block"));
                    }
                    _ => self.at += 1,
                }
            }
            return Err(self.error("Expected } to close a single-line block"));
        }
        if kind == BlockKind::ElseAfterInlineThen {
            return Err(self.error("A same-line else requires a single-line block"));
        }
        let statement_kind = if kind == BlockKind::MacroBody {
            StatementKind::MacroBody
        } else {
            StatementKind::Plain
        };
        while let Some(line) = parser.lines.next() {
            let mut block_line = line.parser();
            if block_line.consume(&TokenKind::CloseBlock) {
                let continued_else = if kind == BlockKind::IfThen && block_line.consume_word("else")
                {
                    Some(block_line.block(parser, depth, BlockKind::Plain)?.body)
                } else {
                    None
                };
                block_line.finish()?;
                return Ok(ParsedBlock {
                    body,
                    continued_else,
                });
            }
            block_line.statement(parser, depth + 1, statement_kind, &mut body)?;
        }
        Err(self.error("Expected } to close the block"))
    }

    fn finish(&self) -> Result<(), Error> {
        if self.at < self.tokens.len() {
            return Err(self.error("Unexpected token after instruction"));
        }
        Ok(())
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
            arguments: self.call_arguments()?,
        })
    }

    fn call_arguments(&mut self) -> Result<Vec<CallArgument>, Error> {
        let mut arguments = Vec::new();
        let mut seen_named = false;
        while let Some(token) = self.tokens.get(self.at) {
            self.space()?;
            if let Some(assign) = self
                .tokens
                .get(self.at + 1)
                .filter(|token| token.kind == TokenKind::Assign)
            {
                let name = &self.source[token.start..token.end];
                if !is_name(name) {
                    return Err(self.error("Expected a parameter name before ="));
                }
                if token.end != assign.start
                    || self.tokens.get(self.at + 2).map(|token| token.start) != Some(assign.end)
                {
                    return Err(self.error("Named arguments require name=value with no spaces"));
                }
                self.at += 2;
                arguments.push(CallArgument::Named {
                    name: name.to_owned(),
                    operand: self.operand()?,
                });
                seen_named = true;
            } else {
                if seen_named {
                    return Err(self.error("Positional arguments must precede named arguments"));
                }
                arguments.push(CallArgument::Positional(self.operand()?));
            }
        }
        Ok(arguments)
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

fn reference_operand(name: String, column: usize) -> Operand {
    Operand {
        text: name.clone(),
        kind: OperandKind::Reference(Path {
            root: name,
            segments: Vec::new(),
        }),
        column,
    }
}

fn reserved_name(name: &str) -> bool {
    matches!(
        name,
        "and"
            | "or"
            | "not"
            | "repeat"
            | "if"
            | "else"
            | "break"
            | "continue"
            | "return"
            | "delay"
            | "collect"
            | "evidence"
    )
}
