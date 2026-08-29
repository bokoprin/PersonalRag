use crate::{
    Corpus, NormalizedText, PrototypeIndex, PrototypeVariant, SearchHit, SearchLimits,
    SearchMetrics, SearchOutcome, collect_set_bits, fold_for_index, normalize_str, normalize_utf8,
    set_bit,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant};

const MAX_PATTERN_CHARS: usize = 8192;
const MAX_NESTING: usize = 64;
const MAX_REPEAT: usize = 4096;
const MAX_NFA_STATES: usize = 16384;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternError(pub String);

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PatternError {}

type PatternResult<T> = Result<T, PatternError>;

#[derive(Clone, Debug)]
pub struct RegexPattern {
    nfa: Nfa,
    mandatory_literal: Option<Vec<u8>>,
    case_sensitive: bool,
}

#[derive(Clone, Debug)]
pub struct WildcardPattern {
    inner: RegexPattern,
}

impl RegexPattern {
    pub fn compile(pattern: &str, case_sensitive: bool) -> PatternResult<Self> {
        if pattern.is_empty() {
            return Err(PatternError("empty regex is rejected".into()));
        }
        if pattern.chars().count() > MAX_PATTERN_CHARS {
            return Err(PatternError("regex exceeds pattern size limit".into()));
        }
        let mut parser = Parser::new(pattern);
        let ast = parser.parse_alt(0)?;
        if parser.peek().is_some() {
            return Err(PatternError("unexpected trailing regex syntax".into()));
        }
        let ast = normalize_ast(ast, case_sensitive)?;
        Self::from_ast(ast, case_sensitive)
    }

    fn from_ast(ast: Ast, case_sensitive: bool) -> PatternResult<Self> {
        let mandatory = mandatory_literal(&ast)
            .filter(|value| !value.is_empty())
            .map(|value| value.into_bytes());
        let nfa = Nfa::compile(&ast)?;
        Ok(Self {
            nfa,
            mandatory_literal: mandatory,
            case_sensitive,
        })
    }

    pub fn mandatory_literal(&self) -> Option<&[u8]> {
        self.mandatory_literal.as_deref()
    }

    pub fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    pub(crate) fn find_starts(&self, raw: &[u8]) -> Vec<u32> {
        let Some(normalized) = normalize_utf8(raw, self.case_sensitive) else {
            return Vec::new();
        };
        self.nfa.find_starts(&normalized)
    }
}

impl WildcardPattern {
    pub fn compile(pattern: &str, case_sensitive: bool) -> PatternResult<Self> {
        if pattern.is_empty() {
            return Err(PatternError("empty wildcard is rejected".into()));
        }
        let mut nodes = Vec::new();
        let mut literal = String::new();
        let mut chars = pattern.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    let escaped = chars
                        .next()
                        .ok_or_else(|| PatternError("dangling wildcard escape".into()))?;
                    literal.push(escaped);
                }
                '*' | '?' => {
                    if !literal.is_empty() {
                        nodes.push(Ast::Literal(std::mem::take(&mut literal)));
                    }
                    if ch == '*' {
                        nodes.push(Ast::Repeat {
                            child: Box::new(Ast::Dot),
                            min: 0,
                            max: None,
                        });
                    } else {
                        nodes.push(Ast::Dot);
                    }
                }
                _ => literal.push(ch),
            }
        }
        if !literal.is_empty() {
            nodes.push(Ast::Literal(literal));
        }
        let ast = normalize_ast(Ast::Concat(nodes), case_sensitive)?;
        Ok(Self {
            inner: RegexPattern::from_ast(ast, case_sensitive)?,
        })
    }

    pub fn mandatory_literal(&self) -> Option<&[u8]> {
        self.inner.mandatory_literal()
    }

    pub fn case_sensitive(&self) -> bool {
        self.inner.case_sensitive()
    }

    pub(crate) fn find_starts(&self, raw: &[u8]) -> Vec<u32> {
        self.inner.find_starts(raw)
    }
}

#[derive(Clone, Debug)]
enum Ast {
    Empty,
    Literal(String),
    Dot,
    Start,
    End,
    Class(CharClass),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        child: Box<Ast>,
        min: usize,
        max: Option<usize>,
    },
}

#[derive(Clone, Debug)]
struct CharClass {
    negated: bool,
    items: Vec<ClassItem>,
}

#[derive(Clone, Debug)]
enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

impl CharClass {
    fn matches(&self, ch: char) -> bool {
        let hit = self.items.iter().any(|item| match *item {
            ClassItem::Range(start, end) => (start..=end).contains(&ch),
            ClassItem::Digit(positive) => ch.is_numeric() == positive,
            ClassItem::Word(positive) => (ch.is_alphanumeric() || ch == '_') == positive,
            ClassItem::Space(positive) => ch.is_whitespace() == positive,
        });
        if self.negated { !hit } else { hit }
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn new(pattern: &str) -> Self {
        Self {
            chars: pattern.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let value = self.peek()?;
        self.pos += 1;
        Some(value)
    }

    fn parse_alt(&mut self, depth: usize) -> PatternResult<Ast> {
        self.check_depth(depth)?;
        let mut branches = vec![self.parse_concat(depth)?];
        while self.peek() == Some('|') {
            self.next();
            branches.push(self.parse_concat(depth)?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Ast::Alt(branches)
        })
    }

    fn parse_concat(&mut self, depth: usize) -> PatternResult<Ast> {
        let mut nodes = Vec::new();
        while let Some(ch) = self.peek() {
            if matches!(ch, ')' | '|') {
                break;
            }
            let atom = self.parse_atom(depth)?;
            nodes.push(self.parse_quantifier(atom)?);
        }
        Ok(match nodes.len() {
            0 => Ast::Empty,
            1 => nodes.pop().unwrap(),
            _ => Ast::Concat(nodes),
        })
    }

    fn parse_atom(&mut self, depth: usize) -> PatternResult<Ast> {
        let ch = self
            .next()
            .ok_or_else(|| PatternError("expected regex atom".into()))?;
        match ch {
            '(' => {
                if self.peek() == Some('?') {
                    return Err(PatternError("special (?...) groups are unsupported".into()));
                }
                let inner = self.parse_alt(depth + 1)?;
                if self.next() != Some(')') {
                    return Err(PatternError("unclosed group".into()));
                }
                Ok(inner)
            }
            ')' => Err(PatternError("unexpected ')'".into())),
            '[' => self.parse_class(),
            '.' => Ok(Ast::Dot),
            '^' => Ok(Ast::Start),
            '$' => Ok(Ast::End),
            '\\' => self.parse_escape(false),
            '*' | '+' | '?' | '{' => Err(PatternError("quantifier without an atom".into())),
            _ => Ok(Ast::Literal(ch.to_string())),
        }
    }

    fn parse_quantifier(&mut self, atom: Ast) -> PatternResult<Ast> {
        let Some(ch) = self.peek() else {
            return Ok(atom);
        };
        let (min, max) = match ch {
            '*' => {
                self.next();
                (0, None)
            }
            '+' => {
                self.next();
                (1, None)
            }
            '?' => {
                self.next();
                (0, Some(1))
            }
            '{' => {
                self.next();
                self.parse_braced_repeat()?
            }
            _ => return Ok(atom),
        };
        if matches!(self.peek(), Some('?') | Some('+')) {
            return Err(PatternError(
                "lazy/possessive quantifiers are unsupported".into(),
            ));
        }
        if min > MAX_REPEAT || max.is_some_and(|value| value > MAX_REPEAT) {
            return Err(PatternError("repeat exceeds safety limit".into()));
        }
        Ok(Ast::Repeat {
            child: Box::new(atom),
            min,
            max,
        })
    }

    fn parse_braced_repeat(&mut self) -> PatternResult<(usize, Option<usize>)> {
        let min = self.parse_number()?;
        match self.next() {
            Some('}') => Ok((min, Some(min))),
            Some(',') => {
                if self.peek() == Some('}') {
                    self.next();
                    return Ok((min, None));
                }
                let max = self.parse_number()?;
                if self.next() != Some('}') || max < min {
                    return Err(PatternError("invalid repeat range".into()));
                }
                Ok((min, Some(max)))
            }
            _ => Err(PatternError("invalid repeat syntax".into())),
        }
    }

    fn parse_number(&mut self) -> PatternResult<usize> {
        let start = self.pos;
        let mut value = 0_usize;
        while let Some(ch) = self.peek() {
            let Some(digit) = ch.to_digit(10) else { break };
            self.next();
            value = value
                .checked_mul(10)
                .and_then(|current| current.checked_add(digit as usize))
                .ok_or_else(|| PatternError("repeat count overflow".into()))?;
        }
        if self.pos == start {
            Err(PatternError("expected repeat count".into()))
        } else {
            Ok(value)
        }
    }

    fn parse_escape(&mut self, in_class: bool) -> PatternResult<Ast> {
        let ch = self
            .next()
            .ok_or_else(|| PatternError("dangling regex escape".into()))?;
        match ch {
            'n' => Ok(Ast::Literal("\n".into())),
            'r' => Ok(Ast::Literal("\r".into())),
            't' => Ok(Ast::Literal("\t".into())),
            'd' => Ok(Ast::Class(CharClass {
                negated: false,
                items: vec![ClassItem::Digit(true)],
            })),
            'D' => Ok(Ast::Class(CharClass {
                negated: false,
                items: vec![ClassItem::Digit(false)],
            })),
            'w' => Ok(Ast::Class(CharClass {
                negated: false,
                items: vec![ClassItem::Word(true)],
            })),
            'W' => Ok(Ast::Class(CharClass {
                negated: false,
                items: vec![ClassItem::Word(false)],
            })),
            's' => Ok(Ast::Class(CharClass {
                negated: false,
                items: vec![ClassItem::Space(true)],
            })),
            'S' => Ok(Ast::Class(CharClass {
                negated: false,
                items: vec![ClassItem::Space(false)],
            })),
            'u' => Ok(Ast::Literal(self.parse_unicode_escape()?.to_string())),
            '0'..='9' if !in_class => Err(PatternError("backreferences are unsupported".into())),
            c if c.is_ascii_alphabetic() => {
                Err(PatternError(format!("unsupported regex escape \\{c}")))
            }
            other => Ok(Ast::Literal(other.to_string())),
        }
    }

    fn parse_unicode_escape(&mut self) -> PatternResult<char> {
        if self.next() != Some('{') {
            return Err(PatternError("Unicode escape must use \\u{HEX}".into()));
        }
        let start = self.pos;
        let mut value = 0_u32;
        while let Some(ch) = self.peek() {
            if ch == '}' {
                break;
            }
            let digit = ch
                .to_digit(16)
                .ok_or_else(|| PatternError("invalid Unicode escape".into()))?;
            self.next();
            value = value
                .checked_mul(16)
                .and_then(|current| current.checked_add(digit))
                .ok_or_else(|| PatternError("Unicode escape overflow".into()))?;
        }
        if self.pos == start || self.next() != Some('}') {
            return Err(PatternError("invalid Unicode escape".into()));
        }
        char::from_u32(value).ok_or_else(|| PatternError("Unicode escape is not a scalar".into()))
    }

    fn parse_class(&mut self) -> PatternResult<Ast> {
        let negated = if self.peek() == Some('^') {
            self.next();
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        while let Some(ch) = self.peek() {
            if ch == ']' && !first {
                self.next();
                return Ok(Ast::Class(CharClass { negated, items }));
            }
            first = false;
            let left = self.parse_class_item()?;
            if self.peek() == Some('-')
                && self
                    .chars
                    .get(self.pos + 1)
                    .copied()
                    .is_some_and(|next| next != ']')
            {
                self.next();
                let right = self.parse_class_item()?;
                match (left, right) {
                    (ClassItem::Range(a, a2), ClassItem::Range(b, b2)) if a == a2 && b == b2 => {
                        if a > b {
                            return Err(PatternError("reversed class range".into()));
                        }
                        items.push(ClassItem::Range(a, b));
                    }
                    _ => return Err(PatternError("class range endpoints must be scalars".into())),
                }
            } else {
                items.push(left);
            }
        }
        Err(PatternError("unclosed character class".into()))
    }

    fn parse_class_item(&mut self) -> PatternResult<ClassItem> {
        let ch = self
            .next()
            .ok_or_else(|| PatternError("unclosed character class".into()))?;
        if ch != '\\' {
            return Ok(ClassItem::Range(ch, ch));
        }
        let escaped = self
            .next()
            .ok_or_else(|| PatternError("dangling class escape".into()))?;
        match escaped {
            'd' => Ok(ClassItem::Digit(true)),
            'D' => Ok(ClassItem::Digit(false)),
            'w' => Ok(ClassItem::Word(true)),
            'W' => Ok(ClassItem::Word(false)),
            's' => Ok(ClassItem::Space(true)),
            'S' => Ok(ClassItem::Space(false)),
            'n' => Ok(ClassItem::Range('\n', '\n')),
            'r' => Ok(ClassItem::Range('\r', '\r')),
            't' => Ok(ClassItem::Range('\t', '\t')),
            'u' => {
                let scalar = self.parse_unicode_escape()?;
                Ok(ClassItem::Range(scalar, scalar))
            }
            c if c.is_ascii_alphabetic() => {
                Err(PatternError(format!("unsupported class escape \\{c}")))
            }
            other => Ok(ClassItem::Range(other, other)),
        }
    }

    fn check_depth(&self, depth: usize) -> PatternResult<()> {
        if depth > MAX_NESTING {
            Err(PatternError("regex nesting exceeds safety limit".into()))
        } else {
            Ok(())
        }
    }
}

fn normalize_ast(ast: Ast, case_sensitive: bool) -> PatternResult<Ast> {
    match ast {
        Ast::Literal(value) => Ok(Ast::Literal(
            String::from_utf8(normalize_str(&value, case_sensitive).into_bytes())
                .expect("normalization emits UTF-8"),
        )),
        Ast::Class(class) => Ok(Ast::Class(normalize_class(class, case_sensitive)?)),
        Ast::Concat(nodes) => {
            let mut out = Vec::new();
            let mut literal = String::new();
            for node in nodes {
                let node = normalize_ast(node, case_sensitive)?;
                if let Ast::Literal(value) = node {
                    literal.push_str(&value);
                } else {
                    if !literal.is_empty() {
                        let normalized = normalize_str(&literal, case_sensitive);
                        out.push(Ast::Literal(
                            String::from_utf8(normalized.into_bytes()).unwrap(),
                        ));
                        literal.clear();
                    }
                    out.push(node);
                }
            }
            if !literal.is_empty() {
                let normalized = normalize_str(&literal, case_sensitive);
                out.push(Ast::Literal(
                    String::from_utf8(normalized.into_bytes()).unwrap(),
                ));
            }
            Ok(Ast::Concat(out))
        }
        Ast::Alt(nodes) => Ok(Ast::Alt(
            nodes
                .into_iter()
                .map(|node| normalize_ast(node, case_sensitive))
                .collect::<PatternResult<Vec<_>>>()?,
        )),
        Ast::Repeat { child, min, max } => Ok(Ast::Repeat {
            child: Box::new(normalize_ast(*child, case_sensitive)?),
            min,
            max,
        }),
        other => Ok(other),
    }
}

fn normalize_class(mut class: CharClass, case_sensitive: bool) -> PatternResult<CharClass> {
    for item in &mut class.items {
        if let ClassItem::Range(start, end) = item {
            *start = normalize_class_scalar(*start, case_sensitive)?;
            *end = normalize_class_scalar(*end, case_sensitive)?;
            if *start > *end {
                return Err(PatternError("normalized class range is reversed".into()));
            }
        }
    }
    Ok(class)
}

fn normalize_class_scalar(ch: char, case_sensitive: bool) -> PatternResult<char> {
    let normalized = normalize_str(&ch.to_string(), case_sensitive).scalar_view();
    if normalized.chars.len() != 1 {
        return Err(PatternError(
            "case-insensitive class element expands to multiple scalars".into(),
        ));
    }
    Ok(normalized.chars[0])
}

fn mandatory_literal(ast: &Ast) -> Option<String> {
    match ast {
        Ast::Literal(value) => (!value.is_empty()).then(|| value.clone()),
        Ast::Concat(nodes) => nodes
            .iter()
            .filter_map(mandatory_literal)
            .max_by_key(String::len),
        Ast::Alt(nodes) => {
            let mut iter = nodes.iter();
            let first = mandatory_literal(iter.next()?)?;
            iter.all(|node| mandatory_literal(node).as_deref() == Some(first.as_str()))
                .then_some(first)
        }
        Ast::Repeat { child, min, .. } if *min > 0 => mandatory_literal(child),
        _ => None,
    }
}

#[derive(Clone, Debug)]
enum Inst {
    Match,
    Consume(Predicate, usize),
    Split(usize, usize),
    AssertStart(usize),
    AssertEnd(usize),
}

#[derive(Clone, Debug)]
enum Predicate {
    Any,
    Char(char),
    Class(CharClass),
}

impl Predicate {
    fn matches(&self, ch: char) -> bool {
        match self {
            Self::Any => true,
            Self::Char(expected) => *expected == ch,
            Self::Class(class) => class.matches(ch),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum EpsilonKind {
    Always,
    Start,
    End,
}

#[derive(Clone, Debug)]
struct Nfa {
    insts: Vec<Inst>,
    start: usize,
    reverse_epsilon: Vec<Vec<(usize, EpsilonKind)>>,
}

impl Nfa {
    fn compile(ast: &Ast) -> PatternResult<Self> {
        let mut insts = vec![Inst::Match];
        let start = compile_ast(ast, 0, &mut insts)?;
        if insts.len() > MAX_NFA_STATES {
            return Err(PatternError("compiled NFA exceeds safety limit".into()));
        }
        let mut reverse_epsilon = vec![Vec::new(); insts.len()];
        for (source, inst) in insts.iter().enumerate() {
            match *inst {
                Inst::Split(a, b) => {
                    reverse_epsilon[a].push((source, EpsilonKind::Always));
                    reverse_epsilon[b].push((source, EpsilonKind::Always));
                }
                Inst::AssertStart(target) => {
                    reverse_epsilon[target].push((source, EpsilonKind::Start));
                }
                Inst::AssertEnd(target) => {
                    reverse_epsilon[target].push((source, EpsilonKind::End));
                }
                Inst::Match | Inst::Consume(_, _) => {}
            }
        }
        Ok(Self {
            insts,
            start,
            reverse_epsilon,
        })
    }

    fn find_starts(&self, text: &NormalizedText) -> Vec<u32> {
        let scalars = text.scalar_view();
        let len = scalars.chars.len();
        let mut next = vec![false; self.insts.len()];
        let mut starts = vec![false; len + 1];
        for pos in (0..=len).rev() {
            let mut current = vec![false; self.insts.len()];
            let mut queue = Vec::new();
            for (state, inst) in self.insts.iter().enumerate() {
                let truth = match inst {
                    Inst::Match => true,
                    Inst::Consume(predicate, target) => {
                        pos < len && predicate.matches(scalars.chars[pos]) && next[*target]
                    }
                    Inst::Split(_, _) | Inst::AssertStart(_) | Inst::AssertEnd(_) => false,
                };
                if truth {
                    current[state] = true;
                    queue.push(state);
                }
            }
            while let Some(target) = queue.pop() {
                for (source, kind) in &self.reverse_epsilon[target] {
                    let enabled = match kind {
                        EpsilonKind::Always => true,
                        EpsilonKind::Start => pos == 0,
                        EpsilonKind::End => pos == len,
                    };
                    if enabled && !current[*source] {
                        current[*source] = true;
                        queue.push(*source);
                    }
                }
            }
            starts[pos] = current[self.start];
            next = current;
        }
        let mut out = Vec::new();
        for (pos, matched) in starts.into_iter().enumerate() {
            if !matched {
                continue;
            }
            let origin = if pos < scalars.origins.len() {
                scalars.origins[pos]
            } else {
                text.original_end()
            };
            if out.last().copied() != Some(origin) {
                out.push(origin);
            }
        }
        out
    }
}

fn compile_ast(ast: &Ast, next: usize, insts: &mut Vec<Inst>) -> PatternResult<usize> {
    if insts.len() > MAX_NFA_STATES {
        return Err(PatternError("compiled NFA exceeds safety limit".into()));
    }
    match ast {
        Ast::Empty => Ok(next),
        Ast::Literal(value) => {
            let mut start = next;
            for ch in value.chars().rev() {
                let index = insts.len();
                insts.push(Inst::Consume(Predicate::Char(ch), start));
                start = index;
            }
            Ok(start)
        }
        Ast::Dot => {
            let index = insts.len();
            insts.push(Inst::Consume(Predicate::Any, next));
            Ok(index)
        }
        Ast::Start => {
            let index = insts.len();
            insts.push(Inst::AssertStart(next));
            Ok(index)
        }
        Ast::End => {
            let index = insts.len();
            insts.push(Inst::AssertEnd(next));
            Ok(index)
        }
        Ast::Class(class) => {
            let index = insts.len();
            insts.push(Inst::Consume(Predicate::Class(class.clone()), next));
            Ok(index)
        }
        Ast::Concat(nodes) => {
            let mut start = next;
            for node in nodes.iter().rev() {
                start = compile_ast(node, start, insts)?;
            }
            Ok(start)
        }
        Ast::Alt(nodes) => {
            let mut starts = nodes
                .iter()
                .map(|node| compile_ast(node, next, insts))
                .collect::<PatternResult<Vec<_>>>()?;
            let mut start = starts.pop().unwrap_or(next);
            while let Some(other) = starts.pop() {
                let index = insts.len();
                insts.push(Inst::Split(other, start));
                start = index;
            }
            Ok(start)
        }
        Ast::Repeat { child, min, max } => {
            let mut start = next;
            match max {
                Some(maximum) => {
                    for _ in *min..*maximum {
                        let child_start = compile_ast(child, start, insts)?;
                        let split = insts.len();
                        insts.push(Inst::Split(child_start, start));
                        start = split;
                    }
                }
                None => {
                    let split = insts.len();
                    insts.push(Inst::Split(usize::MAX, start));
                    let child_start = compile_ast(child, split, insts)?;
                    insts[split] = Inst::Split(child_start, start);
                    start = split;
                }
            }
            for _ in 0..*min {
                start = compile_ast(child, start, insts)?;
            }
            Ok(start)
        }
    }
}

enum CompiledRef<'a> {
    Regex(&'a RegexPattern),
    Wildcard(&'a WildcardPattern),
}

impl CompiledRef<'_> {
    fn mandatory_literal(&self) -> Option<&[u8]> {
        match self {
            Self::Regex(value) => value.mandatory_literal(),
            Self::Wildcard(value) => value.mandatory_literal(),
        }
    }

    fn find_starts(&self, raw: &[u8]) -> Vec<u32> {
        match self {
            Self::Regex(value) => value.find_starts(raw),
            Self::Wildcard(value) => value.find_starts(raw),
        }
    }
}

impl PrototypeIndex {
    pub fn search_regex_all(
        &self,
        corpus: &Corpus,
        pattern: &str,
        case_sensitive: bool,
        variant: PrototypeVariant,
    ) -> PatternResult<SearchOutcome> {
        let compiled = RegexPattern::compile(pattern, case_sensitive)?;
        Ok(self.search_compiled(corpus, CompiledRef::Regex(&compiled), variant, None))
    }

    pub fn search_regex_first_batch(
        &self,
        corpus: &Corpus,
        pattern: &str,
        case_sensitive: bool,
        variant: PrototypeVariant,
    ) -> PatternResult<SearchOutcome> {
        let compiled = RegexPattern::compile(pattern, case_sensitive)?;
        Ok(self.search_compiled(
            corpus,
            CompiledRef::Regex(&compiled),
            variant,
            Some(SearchLimits::default()),
        ))
    }

    pub fn search_wildcard_all(
        &self,
        corpus: &Corpus,
        pattern: &str,
        case_sensitive: bool,
        variant: PrototypeVariant,
    ) -> PatternResult<SearchOutcome> {
        let compiled = WildcardPattern::compile(pattern, case_sensitive)?;
        Ok(self.search_compiled(corpus, CompiledRef::Wildcard(&compiled), variant, None))
    }

    pub fn search_wildcard_first_batch(
        &self,
        corpus: &Corpus,
        pattern: &str,
        case_sensitive: bool,
        variant: PrototypeVariant,
    ) -> PatternResult<SearchOutcome> {
        let compiled = WildcardPattern::compile(pattern, case_sensitive)?;
        Ok(self.search_compiled(
            corpus,
            CompiledRef::Wildcard(&compiled),
            variant,
            Some(SearchLimits::default()),
        ))
    }

    fn search_compiled(
        &self,
        corpus: &Corpus,
        pattern: CompiledRef<'_>,
        variant: PrototypeVariant,
        limits: Option<SearchLimits>,
    ) -> SearchOutcome {
        let filter_started = Instant::now();
        let (candidate_words, global_absent_shortcut, selected_anchor_df, selected_anchor_width) =
            if let Some(anchor) = pattern.mandatory_literal() {
                let folded = fold_for_index(anchor);
                self.candidate_bitmap(&folded, variant)
            } else {
                let mut all = vec![0_u64; self.words_per_bitmap];
                for block_id in 0..self.block_count {
                    set_bit(&mut all, block_id);
                }
                (all, false, None, None)
            };
        let candidate_blocks = collect_set_bits(&candidate_words, self.block_count);
        let candidate_bytes = candidate_blocks
            .iter()
            .map(|block_id| corpus.blocks[*block_id].searchable_bytes)
            .sum();
        let filter_time = filter_started.elapsed();

        let verify_started = Instant::now();
        let mut assembly_time = Duration::ZERO;
        let mut hits = Vec::new();
        let mut verification_bytes = 0_u64;
        let mut seen_files = HashSet::new();
        let mut snippets_per_file = HashMap::<u32, usize>::new();
        let mut matched_locations_seen = 0_usize;
        let mut stop = false;

        'blocks: for block_id in candidate_blocks.iter().copied() {
            for unit_id in &corpus.blocks[block_id].unit_ids {
                let unit = &corpus.units[*unit_id];
                verification_bytes = verification_bytes.saturating_add(unit.raw.len() as u64);
                for original_position in pattern.find_starts(&unit.raw) {
                    matched_locations_seen = matched_locations_seen.saturating_add(1);
                    let assembly_started = Instant::now();
                    seen_files.insert(unit.file_id);
                    let snippet_count = snippets_per_file.entry(unit.file_id).or_default();
                    if limits.is_none_or(|value| *snippet_count < value.max_snippets_per_file) {
                        hits.push(SearchHit {
                            file_id: unit.file_id,
                            line_number: unit.line_number,
                            byte_offset_in_line: original_position,
                        });
                        *snippet_count += 1;
                    }
                    if let Some(value) = limits {
                        stop = seen_files.len() >= value.max_files
                            || matched_locations_seen >= value.max_matches_seen;
                    }
                    assembly_time += assembly_started.elapsed();
                    if stop {
                        break 'blocks;
                    }
                }
            }
        }
        let verify_total = verify_started.elapsed();
        SearchOutcome {
            hits,
            metrics: SearchMetrics {
                candidate_blocks: candidate_blocks.len(),
                candidate_bytes,
                verification_bytes,
                filter_time,
                verify_time: verify_total.saturating_sub(assembly_time),
                result_assembly_time: assembly_time,
                returned_files: seen_files.len(),
                returned_snippets: snippets_per_file.values().sum(),
                matched_locations_seen,
                global_absent_shortcut,
                selected_anchor_df,
                selected_anchor_width,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_rejects_unsafe_features_and_normalizes_escaped_literal_runs() {
        assert!(RegexPattern::compile(r"(a)\1", false).is_err());
        assert!(RegexPattern::compile(r"(?=a)", false).is_err());
        let regex = RegexPattern::compile(r"e\u{301}", true).unwrap();
        assert_eq!(regex.find_starts("é".as_bytes()), vec![0]);
    }

    #[test]
    fn nfa_supports_frozen_core_grammar_without_backtracking() {
        for (pattern, text, expected) in [
            (r"Create(File|Directory)W", "xxCreateDirectoryWyy", vec![2]),
            (r"ERROR_[0-9]{4}", "ERROR_1234 ERROR_abcd", vec![0]),
            (r"^abc$", "abc", vec![0]),
            (r"a.*c", "abbbc", vec![0]),
        ] {
            let regex = RegexPattern::compile(pattern, true).unwrap();
            assert_eq!(regex.find_starts(text.as_bytes()), expected, "{pattern}");
        }
    }

    #[test]
    fn indexed_pattern_search_equals_candidate_free_verification() {
        let mut docs = Vec::new();
        for file_id in 0..4 {
            let mut body = "common filler 日本語 abc|bcd|cde\n".repeat(9000);
            if file_id == 2 {
                body.push_str("UNIQUE_V2_SENTINEL_A1F4 Straße\n");
            }
            docs.push((format!("{file_id}.txt"), body.into_bytes()));
        }
        let corpus = Corpus::from_documents(docs);
        let index = PrototypeIndex::build(&corpus);

        for pattern_text in [r"UNIQUE_V2_SENTINEL_[0-9A-F]{4}", r"STRASSE"] {
            let pattern = RegexPattern::compile(pattern_text, false).unwrap();
            let mut oracle = Vec::new();
            for unit in &corpus.units {
                for offset in pattern.find_starts(&unit.raw) {
                    oracle.push(SearchHit {
                        file_id: unit.file_id,
                        line_number: unit.line_number,
                        byte_offset_in_line: offset,
                    });
                }
            }
            let actual = index
                .search_regex_all(&corpus, pattern_text, false, PrototypeVariant::D)
                .unwrap()
                .hits;
            assert_eq!(actual, oracle, "regex={pattern_text}");
        }

        let wildcard_text = "UNIQUE_V2_SENTINEL_*";
        let wildcard = WildcardPattern::compile(wildcard_text, false).unwrap();
        let mut oracle = Vec::new();
        for unit in &corpus.units {
            for offset in wildcard.find_starts(&unit.raw) {
                oracle.push(SearchHit {
                    file_id: unit.file_id,
                    line_number: unit.line_number,
                    byte_offset_in_line: offset,
                });
            }
        }
        let actual = index
            .search_wildcard_all(&corpus, wildcard_text, false, PrototypeVariant::D)
            .unwrap()
            .hits;
        assert_eq!(actual, oracle);
    }

    #[test]
    fn wildcard_uses_same_nfa_and_escape_rules() {
        let wildcard = WildcardPattern::compile(r"file?.txt", false).unwrap();
        assert_eq!(wildcard.find_starts(b"xxFILE1.TXTyy"), vec![2]);
        let escaped = WildcardPattern::compile(r"literal\*star", true).unwrap();
        assert_eq!(escaped.find_starts(b"literal*star"), vec![0]);
        assert!(WildcardPattern::compile("dangling\\", false).is_err());
    }
}
