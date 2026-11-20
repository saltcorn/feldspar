//! An R expression, parsed with R's own operator precedence (R Language
//! Definition §10.4.2, `?Syntax`), lowest first:
//!
//! ```text
//! =                     right
//! ~                     (one per statement)
//! | ||                  left
//! + -                   left
//! * /                   left
//! %in%                  left
//! :                     left
//! - + (unary)
//! ^                     right
//! ```
//!
//! That order is what makes a formula read as intended without special cases:
//! `y | weights(w) ~ x` is `(y | weights(w)) ~ x`, `(1 + x | g)` is
//! `((1 + x) | g)`, `(1 | p | g)` is `((1 | p) | g)`, and `-1 + x` removes the
//! intercept.

use super::lex::{Tok, Token};
use super::{ParseError, Span};

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Assign,
    Tilde,
    Bar,
    DoubleBar,
    Plus,
    Minus,
    Times,
    Divide,
    In,
    Colon,
    Power,
}

impl BinOp {
    fn from_symbol(s: &str) -> Option<BinOp> {
        Some(match s {
            "=" => BinOp::Assign,
            "~" => BinOp::Tilde,
            "|" => BinOp::Bar,
            "||" => BinOp::DoubleBar,
            "+" => BinOp::Plus,
            "-" => BinOp::Minus,
            "*" => BinOp::Times,
            "/" => BinOp::Divide,
            "%in%" => BinOp::In,
            ":" => BinOp::Colon,
            "^" => BinOp::Power,
            _ => return None,
        })
    }

    /// (left binding power, right binding power).
    fn power(self) -> (u8, u8) {
        match self {
            BinOp::Assign => (2, 1),
            BinOp::Tilde => (3, 4),
            BinOp::Bar | BinOp::DoubleBar => (5, 6),
            BinOp::Plus | BinOp::Minus => (7, 8),
            BinOp::Times | BinOp::Divide => (9, 10),
            BinOp::In => (11, 12),
            BinOp::Colon => (13, 14),
            BinOp::Power => (18, 17),
        }
    }

    pub(crate) fn symbol(self) -> &'static str {
        match self {
            BinOp::Assign => "=",
            BinOp::Tilde => "~",
            BinOp::Bar => "|",
            BinOp::DoubleBar => "||",
            BinOp::Plus => "+",
            BinOp::Minus => "-",
            BinOp::Times => "*",
            BinOp::Divide => "/",
            BinOp::In => "%in%",
            BinOp::Colon => ":",
            BinOp::Power => "^",
        }
    }
}

/// The binding power of a unary minus or plus: above `:`, below `^`.
const UNARY: u8 = 15;

/// One argument of a call: `x`, or `name = x`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Arg {
    pub name: Option<(String, Span)>,
    pub value: Expr,
}

/// An expression, with where it is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Name(String, Span),
    Num(f64, Span),
    Str(String, Span),
    Call {
        name: String,
        name_span: Span,
        args: Vec<Arg>,
        span: Span,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// `-x` (`negative`) or `+x`; and `~ x`, a formula with no left side.
    Unary {
        op: &'static str,
        operand: Box<Expr>,
        span: Span,
    },
    Paren(Box<Expr>, Span),
}

impl Expr {
    pub(crate) fn span(&self) -> Span {
        match self {
            Expr::Name(_, s) | Expr::Num(_, s) | Expr::Str(_, s) | Expr::Paren(_, s) => *s,
            Expr::Call { span, .. } | Expr::Binary { span, .. } | Expr::Unary { span, .. } => *span,
        }
    }

    /// The expression without any brackets around it.
    pub(crate) fn unparen(&self) -> &Expr {
        match self {
            Expr::Paren(inner, _) => inner.unparen(),
            other => other,
        }
    }

    /// The bare name it is.
    pub(crate) fn as_name(&self) -> Option<&str> {
        match self.unparen() {
            Expr::Name(n, _) => Some(n),
            _ => None,
        }
    }

    /// The number it is, `-` included.
    pub(crate) fn as_number(&self) -> Option<f64> {
        match self.unparen() {
            Expr::Num(x, _) => Some(*x),
            Expr::Unary {
                op: "-", operand, ..
            } => operand.as_number().map(|x| -x),
            Expr::Unary {
                op: "+", operand, ..
            } => operand.as_number(),
            _ => None,
        }
    }
}

/// The statements of a token list: each the tokens between breaks, parsed.
pub(crate) fn statements(tokens: &[Token]) -> Vec<Result<Expr, ParseError>> {
    tokens
        .split(|t| t.tok == Tok::Break)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut p = Parser { tokens: s, at: 0 };
            let e = p.expr(0)?;
            if let Some(t) = p.peek() {
                return Err(ParseError::new(
                    format!("unexpected {} after a complete statement", describe(&t.tok)),
                    t.span,
                ));
            }
            Ok(e)
        })
        .collect()
}

/// One expression from `tokens` (a string's contents, re-lexed).
pub(crate) fn expression(tokens: &[Token]) -> Result<Expr, ParseError> {
    let mut parts = statements(tokens);
    match parts.len() {
        1 => parts.remove(0),
        0 => Err(ParseError::new("nothing to read", Span::new(0, 0))),
        _ => Err(ParseError::new(
            "more than one statement",
            tokens.first().map_or(Span::new(0, 0), |t| t.span),
        )),
    }
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Name(n) => format!("`{n}`"),
        Tok::Num(_) => "a number".to_owned(),
        Tok::Str(_) => "a string".to_owned(),
        Tok::Op(o) => format!("`{o}`"),
        Tok::Break => "the end of the line".to_owned(),
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.at);
        self.at += 1;
        t
    }

    fn end_span(&self) -> Span {
        self.tokens
            .last()
            .map_or(Span::new(0, 0), |t| Span::new(t.span.end, t.span.end))
    }

    fn expr(&mut self, min: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.prefix()?;
        while let Some(Token {
            tok: Tok::Op(sym), ..
        }) = self.peek()
        {
            let Some(op) = BinOp::from_symbol(sym) else {
                break;
            };
            let (l, r) = op.power();
            if l < min {
                break;
            }
            self.next();
            let rhs = self.expr(r)?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn prefix(&mut self) -> Result<Expr, ParseError> {
        let end = self.end_span();
        let Some(token) = self.next().cloned() else {
            return Err(ParseError::new("the statement ends too soon", end));
        };
        match token.tok {
            Tok::Num(x) => Ok(Expr::Num(x, token.span)),
            Tok::Str(s) => Ok(Expr::Str(s, token.span)),
            Tok::Name(name) => {
                if matches!(
                    self.peek(),
                    Some(Token {
                        tok: Tok::Op("("),
                        ..
                    })
                ) {
                    self.next();
                    let args = self.args()?;
                    let close = self.tokens[self.at - 1].span;
                    Ok(Expr::Call {
                        name,
                        name_span: token.span,
                        args,
                        span: token.span.to(close),
                    })
                } else {
                    Ok(Expr::Name(name, token.span))
                }
            }
            Tok::Op("(") => {
                let inner = self.expr(0)?;
                match self.next() {
                    Some(Token {
                        tok: Tok::Op(")"),
                        span,
                    }) => {
                        let span = token.span.to(*span);
                        Ok(Expr::Paren(Box::new(inner), span))
                    }
                    Some(t) => Err(ParseError::new(
                        format!("expected `)`, found {}", describe(&t.tok)),
                        t.span,
                    )),
                    None => Err(ParseError::new("this `(` is never closed", token.span)),
                }
            }
            Tok::Op(op @ ("-" | "+")) => {
                let operand = self.expr(UNARY)?;
                let span = token.span.to(operand.span());
                Ok(Expr::Unary {
                    op,
                    operand: Box::new(operand),
                    span,
                })
            }
            Tok::Op("~") => {
                let operand = self.expr(4)?;
                let span = token.span.to(operand.span());
                Ok(Expr::Unary {
                    op: "~",
                    operand: Box::new(operand),
                    span,
                })
            }
            other => Err(ParseError::new(
                format!(
                    "expected a name, a number or `(`, found {}",
                    describe(&other)
                ),
                token.span,
            )),
        }
    }

    /// The arguments after `(`, through the closing `)`.
    fn args(&mut self) -> Result<Vec<Arg>, ParseError> {
        let mut args = Vec::new();
        if matches!(
            self.peek(),
            Some(Token {
                tok: Tok::Op(")"),
                ..
            })
        ) {
            self.next();
            return Ok(args);
        }
        loop {
            // `name = value`: a name (or string) followed by `=`.
            let named = match (self.peek(), self.tokens.get(self.at + 1)) {
                (
                    Some(Token {
                        tok: Tok::Name(n) | Tok::Str(n),
                        span,
                    }),
                    Some(Token {
                        tok: Tok::Op("="), ..
                    }),
                ) => Some((n.clone(), *span)),
                _ => None,
            };
            if named.is_some() {
                self.at += 2;
            }
            // Above `=`, so `a = b` inside a call is the argument's name.
            let value = self.expr(3)?;
            args.push(Arg { name: named, value });
            let end = self.end_span();
            match self.next() {
                Some(Token {
                    tok: Tok::Op(","), ..
                }) => {}
                Some(Token {
                    tok: Tok::Op(")"), ..
                }) => return Ok(args),
                Some(t) => {
                    return Err(ParseError::new(
                        format!("expected `,` or `)`, found {}", describe(&t.tok)),
                        t.span,
                    ));
                }
                None => return Err(ParseError::new("this call is never closed", end)),
            }
        }
    }
}
