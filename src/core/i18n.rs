//! gettext at runtime for the strings the core itself shows (tray menu, notifications).
//!
//! The catalogs are the same as the shell's: `po/<lang>.po` becomes `kuickshare.mo`
//! under `<locale dir>/<lang>/LC_MESSAGES/`. Placeholders follow the style already in
//! the catalogs (`%1`, `%2`, …, counted from 1); the macros replace them in order.
//!
//! The `.mo` reading is a small parser of the format msgfmt writes: 32-bit little-endian
//! (see [`parse_mo`]). A missing catalog keeps the English msgids, the same behaviour as
//! a language without a translation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Context and msgid, the key of the catalog (the context as msgctxt: `ctx\u{4}msgid`).
type Key = (Option<String>, String);

/// The parsed catalog, or the absence of one (translate to the msgid then).
#[derive(Clone)]
pub(crate) struct Catalog {
    messages: HashMap<Key, Form>,
    /// The plural rule of the header; without it English is assumed (0 or 1).
    plural_rule: Option<PluralRule>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Form {
    Single(String),
    Plural(Vec<String>),
}

/// Reads the context in this key space.
fn key(context: Option<&str>, msgid: &str) -> Key {
    (context.map(str::to_owned), msgid.to_owned())
}

/// Loads the catalog once. Called by the macros on their first use; English makes this
/// return `None` and the msgid is used directly.
pub(crate) fn catalog() -> &'static Option<Catalog> {
    static CATALOG: OnceLock<Option<Catalog>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        for language in locales() {
            if let Some(catalog) = catalog_from_language(&language) {
                return Some(catalog);
            }
        }
        None
    })
}

// Public API ----------------------------------------------------------------

/// Translates `msgid` and fills `%1`, `%2`, … with the remaining macro arguments in order.
///
/// Written for string literals only: `po/extract.sh` reads them from the source, so a
/// variable as the msgid is never in a catalog. A msgid with a typographic quote or a
/// non-ASCII character writes it as is (the extraction does not read `\u{…}`). For anything
/// computed there are [`lookup`] and [`lookup_plural`].
macro_rules! i18n {
    ($msgid:expr $(,)?) => {
        $crate::i18n::lookup(None, $msgid, &[])
    };
    ($msgid:expr, $($arg:expr),+ $(,)?) => {
        $crate::i18n::lookup(None, $msgid, &[$($arg),+])
    };
}

/// As [`i18n!`], with a context (`i18nc`): the same msgid under another context is another
/// catalog entry (`"@action:button"`, `"@title:notification"`, …).
macro_rules! i18nc {
    ($ctx:expr, $msgid:expr $(,)?) => {
        $crate::i18n::lookup(Some($ctx), $msgid, &[])
    };
    ($ctx:expr, $msgid:expr, $($arg:expr),+ $(,)?) => {
        $crate::i18n::lookup(Some($ctx), $msgid, &[$($arg),+])
    };
}

/// A plural with a context (`i18ncp`): the msgid and its plural form, and the count `n` that
/// picks the form; the rest of the arguments fill `%1`, `%2`, … in order.
macro_rules! i18ncp {
    ($ctx:expr, $msgid:expr, $msgid_plural:expr, $n:expr $(,)?) => {
        $crate::i18n::lookup_plural(Some($ctx), $msgid, $msgid_plural, $n, &[])
    };
    ($ctx:expr, $msgid:expr, $msgid_plural:expr, $n:expr, $($arg:expr),+ $(,)?) => {
        $crate::i18n::lookup_plural(Some($ctx), $msgid, $msgid_plural, $n, &[$($arg),+])
    };
}

pub(crate) use {i18n, i18nc, i18ncp};

/// Looks up a singular message and substitutes the arguments.
pub fn lookup(context: Option<&str>, msgid: &str, args: &[&str]) -> String {
    let Some(catalog) = catalog() else {
        return substitute(msgid, args);
    };
    let text = match catalog.messages.get(&key(context, msgid)) {
        Some(Form::Single(text)) => text.clone(),
        Some(Form::Plural(_)) => msgid.to_owned(), // a plural entry read as singular
        None => msgid.to_owned(),
    };
    substitute(&text, args)
}

/// Looks up a plural message and substitutes the arguments.
pub fn lookup_plural(
    context: Option<&str>,
    msgid: &str,
    plural: &str,
    n: u64,
    args: &[&str],
) -> String {
    let Some(catalog) = catalog() else {
        let text = if n == 1 { msgid } else { plural };
        return substitute(text, args);
    };
    let index = match &catalog.plural_rule {
        Some(rule) => rule.eval(n) as usize,
        None => usize::from(n != 1),
    };
    let text = match catalog.messages.get(&key(context, msgid)) {
        Some(Form::Plural(forms)) => forms.get(index).cloned().unwrap_or(msgid.to_owned()),
        _ => msgid.to_owned(),
    };
    substitute(&text, args)
}

// Loading -------------------------------------------------------------------

/// The languages to try, from `LANGUAGE`, `LC_ALL`, `LC_MESSAGES` or `LANG`: the first of
/// them that is set wins. Names are stripped of the charset and come with and without
/// their territory: an `it_IT.utf8` looks for `it_IT` and for `it`.
pub fn locales() -> Vec<String> {
    for var in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            let mut names: Vec<String> = Vec::new();
            for part in value.split(':') {
                let part = part.trim();
                if part.is_empty()
                    || part.eq_ignore_ascii_case("C")
                    || part.eq_ignore_ascii_case("POSIX")
                {
                    continue;
                }
                let base = part.split('.').next().unwrap_or(part);
                names.push(base.to_owned());
                if let Some((language, _)) = base.split_once('_') {
                    names.push(language.to_owned());
                }
            }
            if !names.is_empty() {
                return names;
            }
        }
    }
    Vec::new()
}

/// The catalogs of one language, in the order they would be looked at.
fn catalog_paths(language: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(data) = dirs::data_dir() {
        dirs.push(data);
    }
    for dir in std::env::var("XDG_DATA_DIRS")
        .unwrap_or_default()
        .split(':')
    {
        if !dir.is_empty() {
            dirs.push(dir.into());
        }
    }
    dirs.into_iter()
        .map(|data| {
            data.join("locale")
                .join(language)
                .join("LC_MESSAGES")
                .join("kuickshare.mo")
        })
        .collect()
}

fn catalog_from_language(language: &str) -> Option<Catalog> {
    for path in catalog_paths(language) {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Some(catalog) = parse_mo(&bytes) {
                tracing::debug!("translation catalogue: {}", path.display());
                return Some(catalog);
            }
            tracing::warn!("ignoring the unreadable catalogue {}", path.display());
        }
    }
    None
}

// The catalog format --------------------------------------------------------

/// The gettext magic bytes of a little-endian `.mo` file.
const MO_MAGIC: &[u8; 4] = b"\xde\x12\x04\x95";

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.saturating_add(4))?.try_into().ok()?,
    ))
}

/// The C string of a `.mo` entry: the bytes up to the first NUL (the lengths in the
/// string table include the NUL).
fn c_string(bytes: &[u8], at: u32, len: u32) -> Option<&str> {
    let text = bytes.get(at as usize..at.saturating_add(len) as usize)?;
    let text = text.strip_suffix(b"\0").unwrap_or(text);
    str::from_utf8(text).ok()
}

/// Parses a `.mo` file. The header is the first (empty) msgid; it carries the plural rule.
fn parse_mo(bytes: &[u8]) -> Option<Catalog> {
    if bytes.len() < 28 || &bytes[..4] != MO_MAGIC {
        return None;
    }
    let count = u32_at(bytes, 8)? as u64;
    let original_table = u32_at(bytes, 12)? as u64;
    let translated_table = u32_at(bytes, 16)? as u64;
    let mut messages = HashMap::new();
    let mut plural_rule = None;
    for i in 0..count {
        let original_len = u32_at(bytes, (original_table + i * 8) as usize)?;
        let original_at = u32_at(bytes, (original_table + i * 8 + 4) as usize)?;
        let translated_len = u32_at(bytes, (translated_table + i * 8) as usize)?;
        let translated_at = u32_at(bytes, (translated_table + i * 8 + 4) as usize)?;
        let original = c_string(bytes, original_at, original_len)?;
        let translated = c_string(bytes, translated_at, translated_len)?;
        // `msgctxt\u{4}msgid`, or just `msgid` without a context.
        let (context, msgid) = match original.split_once('\u{4}') {
            Some((ctx, id)) => (Some(ctx.to_owned()), id.to_owned()),
            None => (None, original.to_owned()),
        };
        if msgid.is_empty() {
            plural_rule = header_plural_rule(translated);
            continue;
        }
        let forms: Vec<String> = translated.split('\u{0}').map(str::to_owned).collect();
        let form = match forms.len() {
            0 => continue,
            1 => Form::Single(forms.into_iter().next().expect("one at least")),
            _ => Form::Plural(forms),
        };
        messages.insert(key(context.as_deref(), &msgid), form);
    }
    // Without the header entry English plurals are used.
    Some(Catalog {
        messages,
        plural_rule,
    })
}

/// The `Plural-Forms` header line holds `nplurals=N; plural=<expression>;`.
/// The expression is evaluated for whole numbers with `n`, `!`, comparisons,
/// `&&`, `||` and `?:` only, which is what the languages of ours need.
fn header_plural_rule(header: &str) -> Option<PluralRule> {
    for line in header.lines() {
        if let Some(rest) = line.trim().strip_prefix("Plural-Forms:") {
            // `plural` may be written before `nplurals` or on the second line: take the
            // second field whatever the order.
            for field in rest.split(';') {
                let field = field.trim();
                if let Some(expr_text) = field.strip_prefix("plural") {
                    let expr_text = expr_text.trim_start();
                    let expr_text = expr_text
                        .strip_prefix('=')
                        .unwrap_or(expr_text)
                        .trim_start();
                    let mut rest = expr_text;
                    let expr = parse_expr(&mut rest)?;
                    return Some(PluralRule(expr));
                }
            }
        }
    }
    None
}

/// An evaluated plural rule.
#[derive(Clone)]
struct PluralRule(Expr);

impl PluralRule {
    fn eval(&self, n: u64) -> u64 {
        self.0.eval(n)
    }
}

#[derive(Debug, Clone)]
enum Expr {
    /// The number `n` itself; the only variable the rule dialect has.
    N,
    Number(u64),
    Not(Box<Expr>),
    Compare(Box<Expr>, &'static str, Box<Expr>),
    Logical(Box<Expr>, bool, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
}

impl Expr {
    fn eval(&self, n: u64) -> u64 {
        match self {
            Expr::N => n,
            Expr::Number(value) => *value,
            Expr::Not(inner) => u64::from(inner.eval(n) == 0),
            Expr::Compare(l, op, r) => {
                let (a, b) = (l.eval(n), r.eval(n));
                match *op {
                    "==" => u64::from(a == b),
                    "!=" => u64::from(a != b),
                    ">" => u64::from(a > b),
                    "<" => u64::from(a < b),
                    ">=" => u64::from(a >= b),
                    "<=" => u64::from(a <= b),
                    _ => 0,
                }
            }
            Expr::Logical(l, conjunction, r) => {
                let a = l.eval(n);
                let truth = if *conjunction { a != 0 } else { a == 0 };
                if !truth {
                    u64::from(*conjunction)
                } else {
                    u64::from(r.eval(n) != 0)
                }
            }
            Expr::Ternary(c, t, f) => {
                if c.eval(n) != 0 {
                    t.eval(n)
                } else {
                    f.eval(n)
                }
            }
        }
    }
}

fn parse_expr(text: &mut &str) -> Option<Expr> {
    let left = parse_or(text)?;
    *text = text.trim_start();
    if let Some(rest) = text.strip_prefix('?') {
        *text = rest;
        let when_true = parse_expr(text)?;
        *text = text.trim_start().strip_prefix(':')?;
        let when_false = parse_expr(text)?;
        return Some(Expr::Ternary(
            Box::new(left),
            Box::new(when_true),
            Box::new(when_false),
        ));
    }
    Some(left)
}

fn parse_or(text: &mut &str) -> Option<Expr> {
    let mut left = parse_and(text)?;
    loop {
        *text = text.trim_start();
        let Some(rest) = text.strip_prefix("||") else {
            break;
        };
        *text = rest;
        let Some(right) = parse_and(text) else {
            return None; // a `||` with nothing parseable behind it
        };
        left = Expr::Logical(Box::new(left), false, Box::new(right));
    }
    Some(left)
}

fn parse_and(text: &mut &str) -> Option<Expr> {
    let mut left = parse_compare(text)?;
    loop {
        *text = text.trim_start();
        let Some(rest) = text.strip_prefix("&&") else {
            break;
        };
        *text = rest;
        let Some(right) = parse_compare(text) else {
            return None; // a `&&` with nothing parseable behind it
        };
        left = Expr::Logical(Box::new(left), true, Box::new(right));
    }
    Some(left)
}

fn parse_compare(text: &mut &str) -> Option<Expr> {
    let left = parse_primary(text)?;
    *text = text.trim_start();
    for op in ["<=", ">=", "==", "!=", ">", "<"] {
        if let Some(rest) = text.strip_prefix(op) {
            *text = rest;
            let right = parse_compare(text)?;
            return Some(Expr::Compare(Box::new(left), op, Box::new(right)));
        }
    }
    Some(left)
}

fn parse_primary(text: &mut &str) -> Option<Expr> {
    *text = text.trim_start();
    if let Some(rest) = text.strip_prefix('!') {
        *text = rest;
        return Some(Expr::Not(Box::new(parse_primary(text)?)));
    }
    if let Some(rest) = text.strip_prefix('(') {
        *text = rest;
        let inner = parse_expr(text)?;
        *text = text.trim_start().strip_prefix(')')?;
        return Some(inner);
    }
    if let Some(rest) = text.strip_prefix('n') {
        *text = rest;
        return Some(Expr::N);
    }
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    *text = &text[digits.len()..];
    Some(Expr::Number(digits.parse().ok()?))
}

/// Replaces `%1`, `%2`, … with the arguments in order. A number without an argument, or
/// a `%` that does not start one, stays as it is (`File %1 of %2` shows both).
fn substitute(text: &str, args: &[&str]) -> String {
    if args.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 24);
    let mut rest = text;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        // A run of one or two digits follows the `%`: up to 99 arguments.
        let digits: String = rest[1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(2)
            .collect();
        match digits.parse::<usize>() {
            Ok(index) if index >= 1 => {
                rest = &rest[1 + digits.len()..];
                match args.get(index - 1) {
                    Some(arg) => out.push_str(arg),
                    None => {
                        out.push('%');
                        out.push_str(&digits);
                    }
                }
            }
            _ => {
                rest = &rest[1..];
                out.push('%');
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `.mo` order: the string table then the strings, NUL-separated like msgfmt.
    fn mo(entries: &[(&str, &str)]) -> Vec<u8> {
        // The tables point into the file: the strings sit behind the header and both tables.
        let base = (28 + entries.len() * 16) as u32;
        let mut originals: Vec<u8> = Vec::new();
        let mut translations: Vec<u8> = Vec::new();
        let mut blobs: Vec<u8> = Vec::new();
        for (original, translated) in entries {
            originals.extend((original.len() as u32).to_le_bytes());
            originals.extend((base + blobs.len() as u32).to_le_bytes());
            let mut o = original.as_bytes().to_vec();
            o.push(0);
            blobs.extend(o);
            translations.extend((translated.len() as u32).to_le_bytes());
            translations.extend((base + blobs.len() as u32).to_le_bytes());
            let mut t = translated.as_bytes().to_vec();
            t.push(0);
            blobs.extend(t);
        }
        let mut index = originals;
        index.extend(&translations);
        let mut out: Vec<u8> = vec![0; 28];
        out[0..4].copy_from_slice(MO_MAGIC);
        out[8..12].copy_from_slice(&(entries.len() as u32).to_le_bytes()); // count
        out[12..16].copy_from_slice(&28u32.to_le_bytes()); // the table of originals
        out[16..20].copy_from_slice(&((28 + entries.len() * 8) as u32).to_le_bytes()); // the table of translations
        out.extend(index);
        out.extend(blobs);
        out
    }

    #[test]
    fn without_a_catalog_msgids_are_used() {
        for language in locales() {
            // A language of a machine with a catalog would break this test: value the ones
            // the build sets, it sets them before any use of the macros.
            tracing::trace!("locale {language}");
        }
        assert_eq!(lookup(None, "Open KuickShare", &[]), "Open KuickShare");
        assert_eq!(
            lookup(None, "%1 wants to share", &["Alice"]),
            "Alice wants to share"
        );
        assert_eq!(lookup_plural(None, "1 file", "%n files", 1, &[]), "1 file");
        assert_eq!(
            lookup_plural(None, "1 file", "%n files", 3, &[]),
            "%n files"
        );
    }

    #[test]
    fn a_catalog_translates() {
        let bytes = mo(&[
            (
                "",
                "Content-Type: text/plain; charset=UTF-8\nPlural-Forms: nplurals=2; plural=(n != 1);\n",
            ),
            ("Open KuickShare", "Apri KuickShare"),
        ]);
        let catalog = parse_mo(&bytes).expect("a parseable catalog");
        assert_eq!(
            catalog.messages.get(&key(None, "Open KuickShare")),
            Some(&Form::Single("Apri KuickShare".into()))
        );
    }

    #[test]
    fn contexts_separate_the_namespaces() {
        let bytes = mo(&[("@action:button\u{4}Open", "Apri"), ("Open", "Aperto")]);
        let catalog = parse_mo(&bytes).expect("a parseable catalog");
        assert_eq!(
            catalog.messages.get(&key(Some("@action:button"), "Open")),
            Some(&Form::Single("Apri".into()))
        );
        assert_eq!(
            catalog.messages.get(&key(None, "Open")),
            Some(&Form::Single("Aperto".into()))
        );
    }

    fn rule_bytes(rule: &str) -> Vec<u8> {
        let header = format!("Plural-Forms: {rule}\n");
        mo(&[("", &header), ("1 file", "uno\0due\0tre")])
    }

    fn forms_of(rule: &str) -> Vec<String> {
        let catalog = parse_mo(&rule_bytes(rule)).expect("parse");
        let rule = catalog.plural_rule.clone().expect("the rule was read");
        match catalog.messages.get(&key(None, "1 file")) {
            Some(Form::Plural(forms)) => {
                let _ = &rule;
                forms.clone()
            }
            _ => Vec::new(),
        }
    }

    #[test]
    fn plural_rules_evaluate() {
        assert_eq!(
            forms_of("nplurals=2; plural=(n != 1);"),
            vec!["uno", "due", "tre"]
        );
        // The rule of `en`, `it` and `nl`.
        let catalog = parse_mo(&rule_bytes("nplurals=2; plural=(n != 1);")).expect("parse");
        let rule = catalog.plural_rule.as_ref().expect("rule");
        assert_eq!(rule.eval(0), 1);
        assert_eq!(rule.eval(1), 0);
        assert_eq!(rule.eval(2), 1);
        // The rule of `tr`.
        let catalog = parse_mo(&rule_bytes("nplurals=2; plural=(n > 1);")).expect("parse");
        let rule = catalog.plural_rule.as_ref().expect("rule");
        assert_eq!(rule.eval(1), 0);
        assert_eq!(rule.eval(2), 1);
        // A three-form rule.
        let catalog =
            parse_mo(&rule_bytes("nplurals=3; plural=(n==1 ? 0 : n==2 ? 1 : 2);")).expect("parse");
        let rule = catalog.plural_rule.as_ref().expect("rule");
        assert_eq!(rule.eval(1), 0);
        assert_eq!(rule.eval(2), 1);
        assert_eq!(rule.eval(5), 2);
        // One form only.
        let catalog = parse_mo(&rule_bytes("nplurals=1; plural=0;")).expect("parse");
        let rule = catalog.plural_rule.as_ref().expect("rule");
        assert_eq!(rule.eval(77), 0);
    }

    #[test]
    fn plurals_pick_their_form() {
        let bytes = rule_bytes("nplurals=2; plural=(n != 1);");
        let message = parse_mo(&bytes).expect("parse");
        let lookup = |n: u64| match &message.messages.get(&key(None, "1 file")) {
            Some(Form::Plural(forms)) => {
                let rule = message.plural_rule.as_ref().expect("rule");
                forms[rule.eval(n) as usize].clone()
            }
            _ => String::new(),
        };
        assert_eq!(lookup(1), "uno");
        assert_eq!(lookup(3), "due");
    }

    #[test]
    fn substitution_is_ordered() {
        assert_eq!(substitute("From %1", &["the phone"]), "From the phone");
        assert_eq!(substitute("%1 of %2", &["a", "b"]), "a of b");
        assert_eq!(substitute("%2 of %1", &["a", "b"]), "b of a");
        assert_eq!(substitute("no args here", &["a"]), "no args here");
        assert_eq!(substitute("beyond %3", &["a"]), "beyond %3");
        assert_eq!(substitute("a lone % char", &["a"]), "a lone % char");
        assert_eq!(substitute("%1", &[""]), "");
    }
}
