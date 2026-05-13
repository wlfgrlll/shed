pub(super) type Marker = char;


/*
 * These are invisible Unicode characters used to annotate
 * strings with various contextual metadata.
 */

/* Highlight Markers */

// token-level (derived from token class)
pub(super) const COMMAND: Marker = '\u{e100}';
pub(super) const BUILTIN: Marker = '\u{e101}';
pub(super) const ARG: Marker = '\u{e102}';
pub(super) const KEYWORD: Marker = '\u{e103}';
pub(super) const OPERATOR: Marker = '\u{e104}';
pub(super) const REDIRECT: Marker = '\u{e105}';
pub(super) const COMMENT: Marker = '\u{e106}';
pub(super) const ASSIGNMENT: Marker = '\u{e107}';
pub(super) const CMD_SEP: Marker = '\u{e108}';
pub(super) const CASE_PAT: Marker = '\u{e109}';
pub(super) const SUBSH: Marker = '\u{e10a}';
pub(super) const SUBSH_END: Marker = '\u{e10b}';

// sub-token (needs scanning)
pub(super) const VAR_SUB: Marker = '\u{e10c}';
pub(super) const VAR_SUB_END: Marker = '\u{e10d}';
pub(super) const CMD_SUB: Marker = '\u{e10e}';
pub(super) const CMD_SUB_END: Marker = '\u{e10f}';
pub(super) const PROC_SUB: Marker = '\u{e110}';
pub(super) const PROC_SUB_END: Marker = '\u{e111}';
pub(super) const STRING_DQ: Marker = '\u{e112}';
pub(super) const STRING_DQ_END: Marker = '\u{e113}';
pub(super) const STRING_SQ: Marker = '\u{e114}';
pub(super) const STRING_SQ_END: Marker = '\u{e115}';
pub(super) const ESCAPE: Marker = '\u{e116}';
pub(super) const GLOB: Marker = '\u{e117}';
pub(super) const HIST_EXP: Marker = '\u{e11c}';
pub(super) const HIST_EXP_END: Marker = '\u{e11d}';
pub(super) const BACKTICK_SUB: Marker = '\u{e11e}';
pub const BACKTICK_SUB_END: Marker = '\u{e11f}';

// other
pub(super) const VISUAL_MODE_START: Marker = '\u{e118}';
pub(super) const VISUAL_MODE_END: Marker = '\u{e119}';

pub(super) const MATCH_START: Marker = '\u{e120}';
pub(super) const MATCH_END: Marker = '\u{e121}';

pub(super) const RESET: Marker = '\u{e11a}';

pub(super) const NULL: Marker = '\u{e11b}';

/* Expansion Markers */
/// Double quote '"' marker
pub(super) const DUB_QUOTE: Marker = '\u{e001}';
/// Single quote '\\'' marker
pub(super) const SNG_QUOTE: Marker = '\u{e002}';
/// Tilde sub marker
pub(super) const TILDE_SUB: Marker = '\u{e003}';
/// Input process sub marker
pub(super) const PROC_SUB_IN: Marker = '\u{e005}';
/// Output process sub marker
pub(super) const PROC_SUB_OUT: Marker = '\u{e006}';

pub(super) const HEREDOC_START: Marker = '\u{e00a}';
pub(super) const HEREDOC_END: Marker = '\u{e00b}';
pub(super) const HEREDOC_BODY: Marker = '\u{e00c}';
pub(super) const PARAM_OP: Marker = '\u{e00d}'; // parameter expansion operator (##, %, :-, etc.)
pub(super) const PARAM_OP_END: Marker = '\u{e00e}';
pub(super) const PARAM_BODY: Marker = '\u{e00f}'; // pattern/value after operator
pub(super) const PARAM_BODY_END: Marker = '\u{e010}';

/// Marker for null expansion
/// This is used for when "$@" or "$*" are used in quotes and there are no
/// arguments Without this marker, it would be handled like an empty string,
/// which breaks some commands
pub(super) const NULL_EXPAND: Marker = '\u{e007}';

/// Explicit marker for argument separation
/// This is used to join the arguments given by "$@", and preserves exact
/// formatting of the original arguments, including quoting
pub(super) const ARG_SEP: Marker = '\u{e008}';

pub(super) const VI_SEQ_EXP: Marker = '\u{e009}';

pub(super) const END_MARKERS: [Marker; 9] = [
  VAR_SUB_END,
  CMD_SUB_END,
  PROC_SUB_END,
  STRING_DQ_END,
  STRING_SQ_END,
  SUBSH_END,
  PARAM_OP_END,
  PARAM_BODY_END,
  RESET,
];
pub(super) const TOKEN_LEVEL: [Marker; 10] = [
  SUBSH, COMMAND, BUILTIN, ARG, KEYWORD, OPERATOR, REDIRECT, CMD_SEP, CASE_PAT, ASSIGNMENT,
];
pub(super) const SUB_TOKEN: [Marker; 6] = [VAR_SUB, CMD_SUB, PROC_SUB, STRING_DQ, STRING_SQ, GLOB];

pub(super) const MISC: [Marker; 3] = [ESCAPE, VISUAL_MODE_START, VISUAL_MODE_END];

pub(super) fn is_marker(c: Marker) -> bool {
  ('\u{e000}'..'\u{efff}').contains(&c)
}

// Help command formatting markers
pub(super) const TAG: Marker = '\u{e180}';
pub(super) const REFERENCE: Marker = '\u{e181}';
pub(super) const HEADER: Marker = '\u{e182}';
pub(super) const CODE: Marker = '\u{e183}';
/// angle brackets
pub(super) const KEYWORD_1: Marker = '\u{e185}';
/// square brackets
pub(super) const KEYWORD_2: Marker = '\u{e186}';
pub(super) const CODE_BLOCK: Marker = '\u{e187}';

pub(super) fn is_visual_marker(c: Marker) -> bool {
  c == VISUAL_MODE_START || c == VISUAL_MODE_END || c == MATCH_START || c == MATCH_END
}

pub(super) fn strip_markers(str: &str) -> String {
  let mut out = str.to_string();
  out.retain(|c| !is_marker(c));
  out
}
