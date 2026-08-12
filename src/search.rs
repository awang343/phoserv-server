//! Boolean search query parsing for `/api/photos?q=...`.
//!
//! Grammar (case-insensitive keywords, `-term` is shorthand for `NOT term`,
//! juxtaposition without an operator is an implicit AND):
//!
//! ```text
//! expr   := or
//! or     := and (OR and)*
//! and    := not (AND? not)*
//! not    := (NOT | '-') not | atom
//! atom   := '(' or ')' | TERM
//! ```
//!
//! An empty (or whitespace-only) query parses to `Expr::All`, which matches
//! every photo with no filter at all.

use std::collections::{HashMap, HashSet};

use sqlx::SqlitePool;

use crate::tags;

#[derive(Debug, Clone)]
pub enum Expr {
    All,
    Term(String),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Minus,
    Term(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    return Err("unterminated quoted term in search query".to_string());
                }
                if s.is_empty() {
                    return Err("empty quoted term in search query".to_string());
                }
                tokens.push(Token::Term(s));
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && chars[i] != '('
                    && chars[i] != ')'
                    && chars[i] != '"'
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if word.eq_ignore_ascii_case("and") {
                    tokens.push(Token::And);
                } else if word.eq_ignore_ascii_case("or") {
                    tokens.push(Token::Or);
                } else if word.eq_ignore_ascii_case("not") {
                    tokens.push(Token::Not);
                } else if word == "-" {
                    tokens.push(Token::Minus);
                } else if let Some(rest) = word.strip_prefix('-') {
                    tokens.push(Token::Minus);
                    tokens.push(Token::Term(rest.to_string()));
                } else {
                    tokens.push(Token::Term(word));
                }
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    // Juxtaposed terms (no explicit AND/OR between them) are treated as an
    // implicit AND, e.g. `cats dogs` == `cats AND dogs`.
    fn can_start_operand(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Term(_)) | Some(Token::Not) | Some(Token::Minus) | Some(Token::LParen)
        )
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_not()?;
        loop {
            if matches!(self.peek(), Some(Token::And)) {
                self.advance();
            } else if self.can_start_operand() {
                // implicit AND, don't consume a token
            } else {
                break;
            }
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if matches!(self.peek(), Some(Token::Not) | Some(Token::Minus)) {
            self.advance();
            let inner = self.parse_not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<Expr, String> {
        match self.advance() {
            Some(Token::LParen) => {
                let inner = self.parse_or()?;
                match self.advance() {
                    Some(Token::RParen) => Ok(inner),
                    _ => Err("expected closing parenthesis in search query".to_string()),
                }
            }
            Some(Token::Term(t)) => Ok(Expr::Term(t)),
            Some(other) => Err(format!("unexpected token in search query: {other:?}")),
            None => Err("expected a search term".to_string()),
        }
    }
}

/// Parses a raw search query string into a boolean expression tree. An
/// empty/whitespace-only query means "match everything" (`Expr::All`).
pub fn parse(input: &str) -> Result<Expr, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Expr::All);
    }
    let tokens = tokenize(trimmed)?;
    if tokens.is_empty() {
        return Ok(Expr::All);
    }
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err("unexpected trailing input in search query".to_string());
    }
    Ok(expr)
}

fn collect_terms(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::All => {}
        Expr::Term(t) => {
            out.insert(t.clone());
        }
        Expr::And(a, b) | Expr::Or(a, b) => {
            collect_terms(a, out);
            collect_terms(b, out);
        }
        Expr::Not(a) => collect_terms(a, out),
    }
}

/// Resolves every distinct tag-path term in `expr` to its set of matching tag
/// ids (the tag itself plus all descendants), so `build_sql` can run
/// synchronously afterwards. A term with no matching tag resolves to an empty
/// id list (matches nothing).
pub async fn resolve_terms(pool: &SqlitePool, expr: &Expr) -> anyhow::Result<HashMap<String, Vec<i64>>> {
    let mut term_set = HashSet::new();
    collect_terms(expr, &mut term_set);
    let mut result = HashMap::with_capacity(term_set.len());
    for term in term_set {
        let ids = match tags::find_by_path(pool, &term).await? {
            Some(id) => tags::descendant_ids(pool, id).await?,
            None => vec![],
        };
        result.insert(term, ids);
    }
    Ok(result)
}

/// Builds a `p.id`-correlated SQL boolean expression (plus its ordered bind
/// values) from `expr`, using tag ids already resolved by `resolve_terms`.
pub fn build_sql(expr: &Expr, ids_by_term: &HashMap<String, Vec<i64>>) -> (String, Vec<i64>) {
    match expr {
        Expr::All => ("1=1".to_string(), vec![]),
        Expr::Term(t) => {
            let ids = ids_by_term.get(t).cloned().unwrap_or_default();
            if ids.is_empty() {
                ("0=1".to_string(), vec![])
            } else {
                let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                (
                    format!(
                        "EXISTS (SELECT 1 FROM photo_tags pt_term WHERE pt_term.photo_id = p.id AND pt_term.tag_id IN ({placeholders}))"
                    ),
                    ids,
                )
            }
        }
        Expr::And(a, b) => {
            let (sa, mut ba) = build_sql(a, ids_by_term);
            let (sb, bb) = build_sql(b, ids_by_term);
            ba.extend(bb);
            (format!("({sa} AND {sb})"), ba)
        }
        Expr::Or(a, b) => {
            let (sa, mut ba) = build_sql(a, ids_by_term);
            let (sb, bb) = build_sql(b, ids_by_term);
            ba.extend(bb);
            (format!("({sa} OR {sb})"), ba)
        }
        Expr::Not(a) => {
            let (sa, ba) = build_sql(a, ids_by_term);
            (format!("NOT ({sa})"), ba)
        }
    }
}
