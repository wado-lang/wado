//! A small inventory service: a tokenizer and evaluator for a pricing-rule
//! language, an order book, and a report printer.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt::{self, Display};
use std::rc::Rc;
use std::str::FromStr;

const MAX_DEPTH: usize = 64;
static CURRENCY: &str = "EUR";

macro_rules! sku {
    ($cat:literal, $num:expr) => {
        Sku::new(concat!($cat, "-"), $num)
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sku {
    prefix: &'static str,
    number: u32,
}

impl Sku {
    pub const fn new(prefix: &'static str, number: u32) -> Self {
        Self { prefix, number }
    }
}

impl Display for Sku {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{:05}", self.prefix, self.number)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Number(f64),
    Ident(String),
    Str(String),
    Op(char),
    LParen,
    RParen,
    Comma,
    Arrow,
}

#[derive(Debug)]
pub enum RuleError {
    UnexpectedChar { ch: char, pos: usize },
    UnexpectedEnd,
    UnknownVariable(String),
    TooDeep,
    Parse(std::num::ParseFloatError),
}

impl Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleError::UnexpectedChar { ch, pos } => write!(f, "unexpected {ch:?} at {pos}"),
            RuleError::UnexpectedEnd => f.write_str("unexpected end of input"),
            RuleError::UnknownVariable(name) => write!(f, "unknown variable `{name}`"),
            RuleError::TooDeep => write!(f, "expression nests deeper than {MAX_DEPTH}"),
            RuleError::Parse(e) => write!(f, "bad number: {e}"),
        }
    }
}

impl Error for RuleError {}

impl From<std::num::ParseFloatError> for RuleError {
    fn from(e: std::num::ParseFloatError) -> Self {
        RuleError::Parse(e)
    }
}

pub struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    src: &'a str,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer { chars: src.char_indices().peekable(), src }
    }

    fn take_while<F>(&mut self, start: usize, mut pred: F) -> &'a str
    where
        F: FnMut(char) -> bool,
    {
        let mut end = self.src[start..].chars().next().map_or(start, |c| start + c.len_utf8());
        while let Some(&(i, c)) = self.chars.peek() {
            if !pred(c) {
                break;
            }
            end = i + c.len_utf8();
            self.chars.next();
        }
        &self.src[start..end]
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<Token, RuleError>;

    fn next(&mut self) -> Option<Self::Item> {
        let (pos, ch) = loop {
            let (i, c) = self.chars.next()?;
            if !c.is_whitespace() {
                break (i, c);
            }
        };
        let tok = match ch {
            '0'..='9' => {
                let text = self.take_while(pos, |c| c.is_ascii_digit() || c == '.');
                match text.parse::<f64>() {
                    Ok(n) => Token::Number(n),
                    Err(e) => return Some(Err(e.into())),
                }
            }
            'a'..='z' | 'A'..='Z' | '_' => {
                Token::Ident(self.take_while(pos, |c| c.is_alphanumeric() || c == '_').to_owned())
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match self.chars.next() {
                        Some((_, '"')) => break,
                        Some((_, '\\')) => {
                            if let Some((_, esc)) = self.chars.next() {
                                s.push(match esc {
                                    'n' => '\n',
                                    't' => '\t',
                                    other => other,
                                });
                            }
                        }
                        Some((_, c)) => s.push(c),
                        None => return Some(Err(RuleError::UnexpectedEnd)),
                    }
                }
                Token::Str(s)
            }
            '(' => Token::LParen,
            ')' => Token::RParen,
            ',' => Token::Comma,
            '=' if matches!(self.chars.peek(), Some(&(_, '>'))) => {
                self.chars.next();
                Token::Arrow
            }
            '+' | '-' | '*' | '/' | '<' | '>' | '%' => Token::Op(ch),
            _ => return Some(Err(RuleError::UnexpectedChar { ch, pos })),
        };
        Some(Ok(tok))
    }
}

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Var(String),
    Text(String),
    Binary(Box<Expr>, char, Box<Expr>),
    Call(String, Vec<Expr>),
}

pub struct Parser {
    tokens: VecDeque<Token>,
    depth: usize,
}

impl Parser {
    pub fn parse(src: &str) -> Result<Expr, RuleError> {
        let tokens = Lexer::new(src).collect::<Result<VecDeque<_>, _>>()?;
        let mut parser = Parser { tokens, depth: 0 };
        let expr = parser.expr(0)?;
        match parser.tokens.front() {
            None => Ok(expr),
            Some(_) => Err(RuleError::UnexpectedEnd),
        }
    }

    fn precedence(op: char) -> Option<(u8, u8)> {
        Some(match op {
            '<' | '>' => (1, 2),
            '+' | '-' => (3, 4),
            '*' | '/' | '%' => (5, 6),
            _ => return None,
        })
    }

    fn expr(&mut self, min_bp: u8) -> Result<Expr, RuleError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(RuleError::TooDeep);
        }
        let mut lhs = match self.tokens.pop_front().ok_or(RuleError::UnexpectedEnd)? {
            Token::Number(n) => Expr::Num(n),
            Token::Str(s) => Expr::Text(s),
            Token::Ident(name) if self.tokens.front() == Some(&Token::LParen) => {
                self.tokens.pop_front();
                let mut args = Vec::new();
                if self.tokens.front() != Some(&Token::RParen) {
                    loop {
                        args.push(self.expr(0)?);
                        if self.tokens.front() == Some(&Token::Comma) {
                            self.tokens.pop_front();
                        } else {
                            break;
                        }
                    }
                }
                self.tokens.pop_front();
                Expr::Call(name, args)
            }
            Token::Ident(name) => Expr::Var(name),
            Token::LParen => {
                let inner = self.expr(0)?;
                self.tokens.pop_front();
                inner
            }
            other => return Err(RuleError::UnknownVariable(format!("{other:?}"))),
        };
        while let Some(Token::Op(op)) = self.tokens.front().cloned() {
            let Some((l_bp, r_bp)) = Self::precedence(op) else { break };
            if l_bp < min_bp {
                break;
            }
            self.tokens.pop_front();
            let rhs = self.expr(r_bp)?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
        self.depth -= 1;
        Ok(lhs)
    }
}

pub trait Environment {
    fn lookup(&self, name: &str) -> Option<f64>;
}

impl Environment for HashMap<String, f64> {
    fn lookup(&self, name: &str) -> Option<f64> {
        self.get(name).copied()
    }
}

pub fn eval(expr: &Expr, env: &dyn Environment) -> Result<f64, RuleError> {
    Ok(match expr {
        Expr::Num(n) => *n,
        Expr::Text(t) => t.len() as f64,
        Expr::Var(v) => env.lookup(v).ok_or_else(|| RuleError::UnknownVariable(v.clone()))?,
        Expr::Binary(l, op, r) => {
            let (a, b) = (eval(l, env)?, eval(r, env)?);
            match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                '/' if b != 0.0 => a / b,
                '/' => f64::NAN,
                '%' => a % b,
                '<' => (a < b) as u8 as f64,
                '>' => (a > b) as u8 as f64,
                _ => unreachable!("operator {op} has no precedence"),
            }
        }
        Expr::Call(name, args) => {
            let values: Vec<f64> = args.iter().map(|a| eval(a, env)).collect::<Result<_, _>>()?;
            match (name.as_str(), values.as_slice()) {
                ("min", [first, rest @ ..]) => rest.iter().fold(*first, |m, v| m.min(*v)),
                ("max", [first, rest @ ..]) => rest.iter().fold(*first, |m, v| m.max(*v)),
                ("round", [x]) => x.round(),
                ("round", [x, places]) => {
                    let scale = 10f64.powi(*places as i32);
                    (x * scale).round() / scale
                }
                ("if", [cond, then, otherwise]) => {
                    if *cond != 0.0 { *then } else { *otherwise }
                }
                _ => return Err(RuleError::UnknownVariable(name.to_string())),
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub id: u64,
    pub sku: Sku,
    pub side: Side,
    pub quantity: u32,
    pub unit_price: f64,
}

impl FromStr for Side {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "buy" | "b" => Ok(Side::Buy),
            "sell" | "s" => Ok(Side::Sell),
            other => Err(format!("unknown side: {other}")),
        }
    }
}

#[derive(Default)]
pub struct OrderBook {
    orders: Vec<Order>,
    by_sku: BTreeMap<Sku, Vec<usize>>,
    next_id: u64,
}

impl OrderBook {
    pub fn place(&mut self, sku: Sku, side: Side, quantity: u32, unit_price: f64) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.by_sku.entry(sku).or_default().push(self.orders.len());
        self.orders.push(Order { id, sku, side, quantity, unit_price });
        id
    }

    pub fn net_position(&self, sku: &Sku) -> i64 {
        self.by_sku
            .get(sku)
            .into_iter()
            .flatten()
            .map(|&i| &self.orders[i])
            .map(|o| match o.side {
                Side::Buy => o.quantity as i64,
                Side::Sell => -(o.quantity as i64),
            })
            .sum()
    }

    pub fn iter_side(&self, side: Side) -> impl Iterator<Item = &Order> + '_ {
        self.orders.iter().filter(move |o| o.side == side)
    }

    pub fn totals(&self) -> HashMap<Side, f64> {
        let mut totals = HashMap::with_capacity(2);
        for order in &self.orders {
            *totals.entry(order.side).or_insert(0.0) += order.quantity as f64 * order.unit_price;
        }
        totals
    }
}

pub struct Warehouse<T> {
    bins: Vec<Rc<RefCell<Bin<T>>>>,
}

#[derive(Clone, Debug, Default)]
pub struct Bin<T> {
    label: String,
    items: Vec<T>,
}

impl<T: Clone + fmt::Debug> Warehouse<T> {
    pub fn with_bins(count: usize) -> Self {
        let bins = (0..count)
            .map(|i| Rc::new(RefCell::new(Bin { label: format!("B{i:02}"), items: Vec::new() })))
            .collect();
        Warehouse { bins }
    }

    pub fn store(&self, index: usize, item: T) -> Option<usize> {
        let bin = self.bins.get(index)?;
        let mut bin = bin.borrow_mut();
        bin.items.push(item);
        Some(bin.items.len())
    }

    pub fn fullest(&self) -> Option<(String, usize)> {
        self.bins
            .iter()
            .map(|b| {
                let b = b.borrow();
                (b.label.clone(), b.items.len())
            })
            .max_by_key(|(_, n)| *n)
    }
}

fn report<W: fmt::Write>(out: &mut W, book: &OrderBook, skus: &[Sku]) -> fmt::Result {
    writeln!(out, "{:<12} {:>8} {:>12}", "sku", "net", CURRENCY)?;
    let totals = book.totals();
    'rows: for sku in skus {
        let net = book.net_position(sku);
        if net == 0 {
            continue 'rows;
        }
        let value: f64 = book
            .by_sku
            .get(sku)
            .map(|ids| ids.iter().map(|&i| book.orders[i].unit_price).sum::<f64>())
            .unwrap_or_default();
        writeln!(out, "{:<12} {:>8} {:>12.2}", sku.to_string(), net, value)?;
    }
    let buys = totals.get(&Side::Buy).copied().unwrap_or(0.0);
    let sells = totals.get(&Side::Sell).copied().unwrap_or(0.0);
    write!(out, "buy {buys:.2} / sell {sells:.2}")
}

fn distinct_prefixes<'a, I>(skus: I) -> Vec<&'static str>
where
    I: IntoIterator<Item = &'a Sku>,
{
    let mut seen = HashSet::new();
    let mut out: Vec<_> = skus.into_iter().map(|s| s.prefix).filter(|p| seen.insert(*p)).collect();
    out.sort_unstable();
    out
}

fn main() -> Result<(), Box<dyn Error>> {
    let skus = [sku!("TOOL", 17), sku!("TOOL", 4), sku!("PART", 230), sku!("PART", 9)];
    let mut book = OrderBook::default();
    let lines = "buy 0 10 4.5\nsell 0 3 5.0\nb 2 100 0.2\ns 2 40 0.25\nbuy 1 1 99.9";
    for line in lines.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [side, index, qty, price] = fields[..] else {
            return Err(format!("malformed line: {line}").into());
        };
        let side: Side = side.parse()?;
        book.place(skus[index.parse::<usize>()?], side, qty.parse()?, price.parse()?);
    }

    let mut env: HashMap<String, f64> = HashMap::new();
    env.insert("qty".into(), book.net_position(&skus[2]) as f64);
    env.insert("base".into(), 0.2);
    let rule = Parser::parse("round(base * qty - if(qty > 50, qty * 0.01, 0), 2)")?;
    println!("discounted: {}", eval(&rule, &env)?);

    let mut text = String::new();
    report(&mut text, &book, &skus)?;
    println!("{text}");
    println!("prefixes: {:?}", distinct_prefixes(&skus));
    println!("sells: {}", book.iter_side(Side::Sell).count());

    let warehouse: Warehouse<Sku> = Warehouse::with_bins(3);
    for (i, sku) in skus.iter().enumerate() {
        warehouse.store(i % 2, *sku).ok_or("bin out of range")?;
    }
    if let Some((label, n)) = warehouse.fullest() {
        println!("fullest bin {label} holds {n}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_binds_tighter() {
        let env = HashMap::new();
        let e = Parser::parse("1 + 2 * 3").unwrap();
        assert_eq!(eval(&e, &env).unwrap(), 7.0);
    }

    #[test]
    fn rejects_unknown_char() {
        assert!(matches!(Parser::parse("1 # 2"), Err(RuleError::UnexpectedChar { ch: '#', .. })));
    }
}
