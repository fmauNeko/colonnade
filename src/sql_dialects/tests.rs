use super::*;
use gpui_kit::component::{
    Rope,
    highlighter::{LanguageRegistry, SyntaxHighlighter},
};
use gpui_kit::{HighlightStyle, base::input::HighlightStyleResolver, hsla};

const LANGUAGES: [&str; 3] = ["sql-postgres", "sql-mysql", "sql-sqlite"];
const HIGHLIGHT_NAMES: [&str; 41] = [
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constructor",
    "embedded",
    "emphasis",
    "emphasis.strong",
    "enum",
    "function",
    "hint",
    "keyword",
    "label",
    "link_text",
    "link_uri",
    "number",
    "operator",
    "predictive",
    "preproc",
    "primary",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.list_marker",
    "punctuation.special",
    "string",
    "string.escape",
    "string.regex",
    "string.special",
    "string.special.symbol",
    "tag",
    "tag.doctype",
    "text.code.span",
    "text.literal",
    "title",
    "type",
    "variable",
    "variable.special",
    "variant",
];

struct Probe;

impl HighlightStyleResolver for Probe {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        let index = HIGHLIGHT_NAMES
            .iter()
            .position(|candidate| *candidate == name);
        match index {
            Some(index) => Some(HighlightStyle {
                color: Some(hsla(
                    f32::from(u16::try_from(index).unwrap()) / 41.0,
                    1.0,
                    0.5,
                    1.0,
                )),
                ..Default::default()
            }),
            None => name
                .split_once('.')
                .and_then(|(prefix, _)| self.style(prefix)),
        }
    }
}

fn token_style(language: &str, text: &str, token: &str) -> Option<&'static str> {
    let start = text.find(token).unwrap();
    let end = start + token.len();
    let mut highlighter = SyntaxHighlighter::new(language);
    assert!(highlighter.update(None, &Rope::from(text), None));
    let styles = highlighter.styles(&(0..text.len()), &Probe);
    let (_, style) = styles
        .iter()
        .find(|(range, _)| range.start <= start && range.end >= end)?;
    HIGHLIGHT_NAMES
        .iter()
        .copied()
        .find(|name| Probe.style(name).unwrap().color == style.color)
}

#[test]
fn languages_are_registered() {
    // Given the built-in SQL grammar.
    let registry = LanguageRegistry::singleton();
    // When dialect registration runs.
    register_dialects().unwrap();
    // Then every dialect is available.
    for name in LANGUAGES {
        assert!(registry.language(name).is_some(), "{name}");
    }
}

#[test]
fn highlighters_retain_dialect_languages() {
    // Given registered dialects.
    register_dialects().unwrap();
    for name in LANGUAGES {
        // When a real highlighter compiles its query.
        let highlighter = SyntaxHighlighter::new(name);
        // Then invalid-query fallback has not occurred.
        assert_eq!(highlighter.language().as_ref(), name);
    }
}

#[test]
fn common_sql_tokens_resolve_in_every_dialect() {
    // Given a common SQL statement in registered dialects.
    register_dialects().unwrap();
    let text = "SELECT 'a', 1 -- c\nFROM t";
    for language in LANGUAGES {
        for (token, expected) in [
            ("SELECT", "keyword"),
            ("'a'", "string"),
            ("1", "number"),
            ("-- c", "comment"),
        ] {
            // When the token is highlighted.
            let actual = token_style(language, text, token);
            // Then its rendered style resolves to the semantic theme key.
            assert_eq!(actual, Some(expected), "{language}: {token}");
        }
    }
}

#[test]
fn numeric_literals_override_the_base_string_capture() {
    // Given numeric forms accepted by the linked grammar.
    register_dialects().unwrap();
    for language in LANGUAGES {
        for literal in ["0", "42", "1.25", ".5", "5.", "1e3", "0xCAFE", "1_000"] {
            let text = format!("SELECT {literal}");
            // When each literal is highlighted.
            let actual = token_style(language, &text, literal);
            // Then numeric style wins over the upstream catch-all string capture.
            assert_eq!(actual, Some("number"), "{language}: {literal}");
        }
    }
}

#[test]
fn quoted_numbers_retain_the_string_style() {
    // Given quoted numeric text that must not match the numeric override.
    register_dialects().unwrap();
    for language in LANGUAGES {
        for literal in ["'1'", "'1.25'", "'-.5'", "'1e3'", "'0xCAFE'"] {
            let text = format!("SELECT {literal}");
            // When each quoted literal is highlighted.
            let actual = token_style(language, &text, literal);
            // Then its string style is preserved.
            assert_eq!(actual, Some("string"), "{language}: {literal}");
        }
    }
}

#[test]
fn postgres_types_are_dialect_specific() {
    // Given a PostgreSQL type name in an identifier position unstyled by the base query.
    register_dialects().unwrap();
    let text = "CREATE TABLE t (citext text)";
    // When both dialects highlight it.
    let styles = ["sql-postgres", "sql-mysql"].map(|lang| token_style(lang, text, "citext"));
    // Then only PostgreSQL assigns the type style.
    assert_eq!(styles[0], Some("type"));
    assert_ne!(styles[1], Some("type"));
}

#[test]
fn mysql_keywords_are_dialect_specific() {
    // Given a MySQL keyword in an identifier position supported by the grammar.
    register_dialects().unwrap();
    let text = "CREATE TABLE t (REGEXP text)";
    // When both dialects highlight it.
    let styles = ["sql-mysql", "sql-postgres"].map(|lang| token_style(lang, text, "REGEXP"));
    // Then only MySQL assigns the keyword style.
    assert_eq!(styles[0], Some("keyword"));
    assert_ne!(styles[1], Some("keyword"));
}

#[test]
fn sqlite_keywords_are_dialect_specific() {
    // Given a SQLite keyword in an identifier position supported by the grammar.
    register_dialects().unwrap();
    let text = "CREATE TABLE t (PRAGMA text)";
    // When both dialects highlight it.
    let styles = ["sql-sqlite", "sql-postgres"].map(|lang| token_style(lang, text, "PRAGMA"));
    // Then only SQLite assigns the keyword style.
    assert_eq!(styles[0], Some("keyword"));
    assert_ne!(styles[1], Some("keyword"));
}

#[test]
fn every_capture_resolves_to_a_theme_key() {
    // Given registered dialect queries and their capture syntax.
    register_dialects().unwrap();
    let captures = regex::Regex::new(r"@([\w.]+)").unwrap();
    for language in LANGUAGES {
        // When every capture in the final query is inspected.
        let config = LanguageRegistry::singleton().language(language).unwrap();
        for capture in captures.captures_iter(&config.highlights) {
            let name = &capture[1];
            let prefix = name.split('.').next().unwrap();
            // Then its first segment is supported by Kit.
            assert!(HIGHLIGHT_NAMES.contains(&prefix), "{language}: @{name}");
        }
    }
}

#[test]
fn registration_is_repeatable() {
    // Given previously registered dialects.
    register_dialects().unwrap();
    // When registration runs again.
    let result = register_dialects();
    // Then it succeeds.
    assert!(result.is_ok());
}

#[test]
fn capture_rewrite_preserves_boundaries_and_suffixes() {
    // Given upstream captures, including similarly prefixed names.
    let query =
        "@conditional @storageclass @field @parameter @float @spell @fieldish @field.member";
    // When unsupported captures are rewritten.
    let rewritten = rewrite_captures(query);
    // Then only whole capture roots change and spell is removed.
    assert_eq!(
        rewritten,
        "@keyword @keyword @property @variable.special @number  @fieldish @property.member"
    );
}
