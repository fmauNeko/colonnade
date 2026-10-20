//! Dialect-aware SQL highlighting languages.

use gpui_kit::component::highlighter::LanguageRegistry;
use regex::{Captures, Regex};

/// Failure to register SQL dialect grammars.
#[derive(Debug, thiserror::Error)]
pub enum DialectError {
    /// Kit was built without its SQL grammar.
    #[error("the built-in SQL grammar is unavailable")]
    MissingBaseGrammar,
}

/// Registers PostgreSQL, MySQL and SQLite highlighting languages, safely on repeated calls.
///
/// Normalizes the runtime-loaded base query with [`rewrite_captures`] and a
/// leading numeric-literal rule. The upstream numeric predicates use Lua `%d`,
/// but Kit evaluates regexes; its catch-all literal rule otherwise wins as string.
/// Kit 0.7.3 keeps the first capture for adjacent identical ranges, so the rule
/// must precede the base query. The grammar validates literals; a numeric prefix
/// distinguishes numbers (including exponents and radix forms) from quoted text.
///
/// # Errors
/// Returns [`DialectError::MissingBaseGrammar`] if Kit's SQL grammar is unavailable.
pub fn register_dialects() -> Result<(), DialectError> {
    let registry = LanguageRegistry::singleton();
    let base = registry
        .language("sql")
        .ok_or(DialectError::MissingBaseGrammar)?;
    let highlights = format!(
        "((literal) @number (#match? @number \"^[-+]?([0-9]|[.][0-9])\"))\n{}",
        rewrite_captures(&base.highlights)
    );
    for (name, extra) in [
        ("sql-postgres", include_str!("postgres.scm")),
        ("sql-mysql", include_str!("mysql.scm")),
        ("sql-sqlite", include_str!("sqlite.scm")),
    ] {
        let mut config = base.clone();
        config.name = name.into();
        config.highlights = format!("{highlights}\n{extra}").into();
        registry.register(name, &config);
    }
    Ok(())
}

/// Rewrites upstream SQL capture names to Kit's supported semantic theme keys.
#[must_use]
pub fn rewrite_captures(query: &str) -> String {
    let regex = match Regex::new(r"@(conditional|storageclass|field|parameter|float|spell)\b") {
        Ok(regex) => regex,
        Err(error) => unreachable!("the fixed capture regex is valid: {error}"),
    };
    regex
        .replace_all(query, |captures: &Captures<'_>| match &captures[1] {
            "conditional" | "storageclass" => "@keyword",
            "field" => "@property",
            "parameter" => "@variable.special",
            "float" => "@number",
            "spell" => "",
            _ => unreachable!("the regex only captures the six listed names"),
        })
        .into_owned()
}

#[cfg(test)]
mod tests;
