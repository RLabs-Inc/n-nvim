//! Editor options — the `:set` system.
//!
//! Provides parsed `:set` directives and option name validation for the
//! Vim-compatible `:set` command. Option values themselves live on the
//! [`Editor`] and [`View`] structs — this module handles the parsing layer.
//!
//! # Supported syntax
//!
//! | Syntax           | Effect                        |
//! |------------------|-------------------------------|
//! | `:set option`    | Enable boolean / show numeric |
//! | `:set nooption`  | Disable boolean               |
//! | `:set option!`   | Toggle boolean                |
//! | `:set option?`   | Query current value           |
//! | `:set option=N`  | Assign numeric value          |
//! | `:set`           | Show changed options          |
//! | `:set all`       | Show all options              |
//!
//! # Option names
//!
//! Both full names and Vim abbreviations are accepted:
//!
//! | Full name        | Abbrev | Type    | Default |
//! |------------------|--------|---------|---------|
//! | `number`         | `nu`   | bool    | true    |
//! | `relativenumber` | `rnu`  | bool    | false   |
//! | `scrolloff`      | `so`   | integer | 0       |
//! | `tabstop`        | `ts`   | integer | 4       |
//! | `shiftwidth`     | `sw`   | integer | 4       |
//! | `expandtab`      | `et`   | bool    | true    |
//! | `ignorecase`     | `ic`   | bool    | false   |
//! | `smartcase`      | `scs`  | bool    | false   |
//! | `hlsearch`       | `hls`  | bool    | true    |
//! | `incsearch`      | `is`   | bool    | true    |
//! | `wrapscan`       | `ws`   | bool    | true    |
//! | `cursorline`     | `cul`  | bool    | false   |

/// A parsed `:set` directive.
///
/// Produced by [`parse_set`] from the arguments to `:set`. The editor
/// interprets these to read or modify option values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetDirective {
    /// `:set option` — enable a boolean option.
    On(String),

    /// `:set nooption` — disable a boolean option.
    Off(String),

    /// `:set option!` — toggle a boolean option.
    Toggle(String),

    /// `:set option?` — query the current value.
    Query(String),

    /// `:set option=value` — assign a value.
    Assign(String, String),

    /// `:set` with no arguments — show changed options.
    ShowChanged,

    /// `:set all` — show all options.
    ShowAll,
}

/// Returns `true` if `name` is a known boolean option (full name or abbreviation).
#[must_use]
pub fn is_bool_option(name: &str) -> bool {
    matches!(
        name,
        "number"
            | "nu"
            | "relativenumber"
            | "rnu"
            | "expandtab"
            | "et"
            | "ignorecase"
            | "ic"
            | "smartcase"
            | "scs"
            | "hlsearch"
            | "hls"
            | "incsearch"
            | "is"
            | "wrapscan"
            | "ws"
            | "cursorline"
            | "cul"
    )
}

/// Returns `true` if `name` is a known numeric option (full name or abbreviation).
#[must_use]
pub fn is_numeric_option(name: &str) -> bool {
    matches!(
        name,
        "scrolloff" | "so" | "tabstop" | "ts" | "shiftwidth" | "sw"
    )
}

/// Returns `true` if `name` is any known option (boolean or numeric).
#[must_use]
pub fn is_known_option(name: &str) -> bool {
    is_bool_option(name) || is_numeric_option(name)
}

/// Parse the full `:set` arguments string into directives.
///
/// Multiple space-separated arguments are supported (e.g., `:set number scrolloff=5`).
/// An empty argument string produces [`SetDirective::ShowChanged`].
#[must_use]
pub fn parse_set(args: &str) -> Vec<SetDirective> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return vec![SetDirective::ShowChanged];
    }
    trimmed.split_whitespace().map(parse_set_arg).collect()
}

/// Parse a single `:set` argument into a directive.
#[must_use]
pub fn parse_set_arg(arg: &str) -> SetDirective {
    if arg == "all" {
        return SetDirective::ShowAll;
    }

    // Assignment: option=value
    if let Some(eq_pos) = arg.find('=') {
        let name = &arg[..eq_pos];
        let value = &arg[eq_pos + 1..];
        return SetDirective::Assign(name.to_string(), value.to_string());
    }

    // Query: option?
    if let Some(name) = arg.strip_suffix('?') {
        return SetDirective::Query(name.to_string());
    }

    // Toggle: option!
    if let Some(name) = arg.strip_suffix('!') {
        return SetDirective::Toggle(name.to_string());
    }

    // Negation: nooption — only if the remainder is a known boolean option.
    // This avoids mis-parsing `:set number` as Off("mber") since "number"
    // starts with "no".
    if let Some(name) = arg.strip_prefix("no") {
        if !name.is_empty() && is_bool_option(name) {
            return SetDirective::Off(name.to_string());
        }
    }

    // Bare numeric option name = query its value (Vim behavior).
    if is_numeric_option(arg) {
        return SetDirective::Query(arg.to_string());
    }

    // Default: enable boolean option.
    SetDirective::On(arg.to_string())
}

/// Format a boolean option for display (`:set` output).
///
/// Returns `"name"` when true, `"noname"` when false.
#[must_use]
pub fn format_bool(name: &str, value: bool) -> String {
    if value {
        name.to_string()
    } else {
        format!("no{name}")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_set_arg ─────────────────────────────────────────────────────

    #[test]
    fn parse_boolean_on() {
        assert_eq!(parse_set_arg("number"), SetDirective::On("number".into()));
        assert_eq!(parse_set_arg("nu"), SetDirective::On("nu".into()));
        assert_eq!(
            parse_set_arg("relativenumber"),
            SetDirective::On("relativenumber".into())
        );
    }

    #[test]
    fn parse_boolean_off() {
        assert_eq!(
            parse_set_arg("nonumber"),
            SetDirective::Off("number".into())
        );
        assert_eq!(parse_set_arg("nonu"), SetDirective::Off("nu".into()));
        assert_eq!(
            parse_set_arg("norelativenumber"),
            SetDirective::Off("relativenumber".into())
        );
        assert_eq!(parse_set_arg("nornu"), SetDirective::Off("rnu".into()));
    }

    #[test]
    fn parse_toggle() {
        assert_eq!(
            parse_set_arg("number!"),
            SetDirective::Toggle("number".into())
        );
        assert_eq!(
            parse_set_arg("rnu!"),
            SetDirective::Toggle("rnu".into())
        );
    }

    #[test]
    fn parse_query() {
        assert_eq!(
            parse_set_arg("number?"),
            SetDirective::Query("number".into())
        );
        assert_eq!(
            parse_set_arg("scrolloff?"),
            SetDirective::Query("scrolloff".into())
        );
    }

    #[test]
    fn parse_assign() {
        assert_eq!(
            parse_set_arg("scrolloff=5"),
            SetDirective::Assign("scrolloff".into(), "5".into())
        );
        assert_eq!(
            parse_set_arg("ts=8"),
            SetDirective::Assign("ts".into(), "8".into())
        );
        assert_eq!(
            parse_set_arg("shiftwidth=2"),
            SetDirective::Assign("shiftwidth".into(), "2".into())
        );
    }

    #[test]
    fn parse_show_all() {
        assert_eq!(parse_set_arg("all"), SetDirective::ShowAll);
    }

    #[test]
    fn parse_numeric_bare_is_query() {
        // Bare numeric option name = query, not enable.
        assert_eq!(
            parse_set_arg("scrolloff"),
            SetDirective::Query("scrolloff".into())
        );
        assert_eq!(
            parse_set_arg("tabstop"),
            SetDirective::Query("tabstop".into())
        );
        assert_eq!(parse_set_arg("so"), SetDirective::Query("so".into()));
        assert_eq!(parse_set_arg("ts"), SetDirective::Query("ts".into()));
        assert_eq!(parse_set_arg("sw"), SetDirective::Query("sw".into()));
    }

    #[test]
    fn parse_number_not_confused_with_no_prefix() {
        // "number" starts with "no" but "mber" isn't a known option.
        assert_eq!(parse_set_arg("number"), SetDirective::On("number".into()));
    }

    #[test]
    fn parse_unknown_option() {
        // Unknown options still parse (editor reports error at apply time).
        assert_eq!(
            parse_set_arg("foobar"),
            SetDirective::On("foobar".into())
        );
        assert_eq!(
            parse_set_arg("nofoobar"),
            SetDirective::On("nofoobar".into()) // "foobar" not a bool option
        );
    }

    // ── parse_set (multiple args) ────────────────────────────────────────

    #[test]
    fn parse_empty_is_show_changed() {
        assert_eq!(parse_set(""), vec![SetDirective::ShowChanged]);
        assert_eq!(parse_set("  "), vec![SetDirective::ShowChanged]);
    }

    #[test]
    fn parse_multiple_args() {
        let result = parse_set("number scrolloff=5 nohlsearch");
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], SetDirective::On("number".into()));
        assert_eq!(
            result[1],
            SetDirective::Assign("scrolloff".into(), "5".into())
        );
        assert_eq!(result[2], SetDirective::Off("hlsearch".into()));
    }

    // ── Abbreviations ────────────────────────────────────────────────────

    #[test]
    fn abbreviations_boolean() {
        assert!(is_bool_option("nu"));
        assert!(is_bool_option("rnu"));
        assert!(is_bool_option("et"));
        assert!(is_bool_option("ic"));
        assert!(is_bool_option("scs"));
        assert!(is_bool_option("hls"));
        assert!(is_bool_option("is"));
        assert!(is_bool_option("ws"));
        assert!(is_bool_option("cul"));
    }

    #[test]
    fn abbreviations_numeric() {
        assert!(is_numeric_option("so"));
        assert!(is_numeric_option("ts"));
        assert!(is_numeric_option("sw"));
    }

    #[test]
    fn unknown_is_not_option() {
        assert!(!is_known_option("foobar"));
        assert!(!is_known_option("mber")); // not confused with "number"
    }

    // ── format_bool ──────────────────────────────────────────────────────

    #[test]
    fn format_bool_on_off() {
        assert_eq!(format_bool("number", true), "number");
        assert_eq!(format_bool("number", false), "nonumber");
        assert_eq!(format_bool("hlsearch", true), "hlsearch");
        assert_eq!(format_bool("hlsearch", false), "nohlsearch");
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn parse_nois_disables_incsearch() {
        // "nois" → strip "no" → "is" → is_bool_option("is") → true → Off("is")
        assert_eq!(parse_set_arg("nois"), SetDirective::Off("is".into()));
    }

    #[test]
    fn parse_noet_disables_expandtab() {
        assert_eq!(parse_set_arg("noet"), SetDirective::Off("et".into()));
    }

    #[test]
    fn parse_assign_zero() {
        assert_eq!(
            parse_set_arg("scrolloff=0"),
            SetDirective::Assign("scrolloff".into(), "0".into())
        );
    }
}
