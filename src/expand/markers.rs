pub(crate) type Marker = char;

/*
 * These are invisible Unicode characters used to annotate
 * strings with various contextual metadata.
 */

/* Highlight Markers */

// token-level (derived from token class)
pub(crate) const SUBSH: Marker = '\u{e10a}';

// sub-token (needs scanning)
pub(crate) const VAR_SUB: Marker = '\u{e10c}';
pub(crate) const ESCAPE: Marker = '\u{e116}';

// other
pub(crate) const RESET: Marker = '\u{e11a}';

/* Expansion Markers */
/// Double quote '"' marker
pub(crate) const DUB_QUOTE: Marker = '\u{e001}';
/// Single quote '\\'' marker
pub(crate) const SNG_QUOTE: Marker = '\u{e002}';
/// Tilde sub marker
pub(crate) const TILDE_SUB: Marker = '\u{e003}';
/// Input process sub marker
pub(crate) const PROC_SUB_IN: Marker = '\u{e005}';
/// Output process sub marker
pub(crate) const PROC_SUB_OUT: Marker = '\u{e006}';

/// Marker for null expansion
/// This is used for when "$@" or "$*" are used in quotes and there are no
/// arguments Without this marker, it would be handled like an empty string,
/// which breaks some commands
pub(crate) const NULL_EXPAND: Marker = '\u{e007}';

/// Explicit marker for argument separation
/// This is used to join the arguments given by "$@", and preserves exact
/// formatting of the original arguments, including quoting
pub(crate) const ARG_SEP: Marker = '\u{e008}';

pub(crate) fn is_marker(c: Marker) -> bool {
  ('\u{e000}'..'\u{efff}').contains(&c)
}

// Help command formatting markers
pub(crate) const TAG: Marker = '\u{e180}';
pub(crate) const REFERENCE: Marker = '\u{e181}';
pub(crate) const HEADER: Marker = '\u{e182}';
pub(crate) const CODE: Marker = '\u{e183}';
/// angle brackets
pub(crate) const KEYWORD_1: Marker = '\u{e185}';
/// square brackets
pub(crate) const KEYWORD_2: Marker = '\u{e186}';

pub(crate) fn strip_markers(str: &str) -> String {
  let mut out = str.to_string();
  out.retain(|c| !is_marker(c));
  out
}
