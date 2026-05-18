#![allow(non_snake_case)]

use super::{Prompt, Shed, ShedLine, eval::lex::Span, key};

use crate::tests::testutil::TestGuard;

/// Tests for our vim logic emulation. Each test consists of an initial text, a sequence of keys to feed, and the expected final text and cursor position.
macro_rules! vi_test {
  { $($name:ident: $input:expr => $op:expr => $expected_text:expr,$expected_cursor:expr);* $(;)? } => {
    mod vi {
      use super::*;
      $(#[test]
        fn $name() {
          let (mut vi, _g) = test_vi($input);
          Shed::term_mut(|t| t.feed_bytes(b"\x1b")); // Start in normal mode
          let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
          vi.process_input(keys).unwrap();

          for byte in $op.as_bytes() {
            Shed::term_mut(|t| t.feed_bytes(&[*byte]));
            let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
            vi.process_input(keys).unwrap();
          }
          assert_eq!(vi.editor.joined(), $expected_text);
          assert_eq!(vi.editor.cursor_to_flat(), $expected_cursor);
        }
      )*
    }
  };
}

// Why can't I marry a programming language
vi_test! {
  // test function name    // initial buffer                // command sequence      // final buffer, cursor position
  dw_basic                 : "hello world"                  => "dw"                  => "world", 0;
  dw_middle                : "one two three"                => "wdw"                 => "one three", 4;
  dd_whole_line            : "hello world"                  => "dd"                  => "", 0;
  x_single                 : "hello"                        => "x"                   => "ello", 0;
  x_middle                 : "hello"                        => "llx"                 => "helo", 2;
  X_backdelete             : "hello"                        => "llX"                 => "hllo", 1;
  h_motion                 : "hello"                        => "$h"                  => "hello", 3;
  l_motion                 : "hello"                        => "l"                   => "hello", 1;
  h_at_start               : "hello"                        => "h"                   => "hello", 0;
  l_at_end                 : "hello"                        => "$l"                  => "hello", 4;
  w_forward                : "one two three"                => "w"                   => "one two three", 4;
  b_backward               : "one two three"                => "$b"                  => "one two three", 8;
  e_end                    : "one two three"                => "e"                   => "one two three", 2;
  ge_back_end              : "one two three"                => "$ge"                 => "one two three", 6;
  w_punctuation            : "foo.bar baz"                  => "w"                   => "foo.bar baz", 3;
  e_punctuation            : "foo.bar baz"                  => "e"                   => "foo.bar baz", 2;
  b_punctuation            : "foo.bar baz"                  => "$b"                  => "foo.bar baz", 8;
  w_at_eol                 : "hello"                        => "$w"                  => "hello", 4;
  b_at_bol                 : "hello"                        => "b"                   => "hello", 0;
  W_forward                : "foo.bar baz"                  => "W"                   => "foo.bar baz", 8;
  B_backward               : "foo.bar baz"                  => "$B"                  => "foo.bar baz", 8;
  E_end                    : "foo.bar baz"                  => "E"                   => "foo.bar baz", 6;
  gE_back_end              : "one two three"                => "$gE"                 => "one two three", 6;
  W_skip_punct             : "one-two three"                => "W"                   => "one-two three", 8;
  B_skip_punct             : "one two-three"                => "$B"                  => "one two-three", 4;
  E_skip_punct             : "one-two three"                => "E"                   => "one-two three", 6;
  dW_big                   : "foo.bar baz"                  => "dW"                  => "baz", 0;
  cW_big                   : "foo.bar baz"                  => "cWx\x1b"             => "x baz", 0;
  zero_bol                 : "  hello"                      => "$0"                  => "  hello", 0;
  caret_first_char         : "  hello"                      => "$^"                  => "  hello", 2;
  dollar_eol               : "hello world"                  => "$"                   => "hello world", 10;
  g_last_nonws             : "hello   "                     => "g_"                  => "hello   ", 4;
  g_no_trailing            : "hello"                        => "g_"                  => "hello", 4;
  pipe_column              : "hello world"                  => "6|"                  => "hello world", 5;
  pipe_col1                : "hello world"                  => "1|"                  => "hello world", 0;
  I_insert_front           : "  hello"                      => "Iworld \x1b"         => "  world hello", 7;
  A_append_end             : "hello"                        => "A world\x1b"         => "hello world", 10;
  f_find                   : "hello world"                  => "fo"                  => "hello world", 4;
  F_find_back              : "hello world"                  => "$Fo"                 => "hello world", 7;
  t_till                   : "hello world"                  => "tw"                  => "hello world", 5;
  T_till_back              : "hello world"                  => "$To"                 => "hello world", 8;
  f_no_match               : "hello"                        => "fz"                  => "hello", 0;
  semicolon_repeat         : "abcabc"                       => "fa;;"                => "abcabc", 3;
  comma_reverse            : "abcabc"                       => "fa;;,"               => "abcabc", 0;
  df_semicolon             : "abcabc"                       => "fa;;dfa"             => "abcabc", 3;
  t_at_target              : "aab"                          => "lta"                 => "aab", 1;
  D_to_end                 : "hello world"                  => "wD"                  => "hello ", 5;
  d_dollar                 : "hello world"                  => "wd$"                 => "hello ", 5;
  d0_to_start              : "hello world"                  => "$d0"                 => "d", 0;
  dw_multiple              : "one two three"                => "d2w"                 => "three", 0;
  dt_char                  : "hello world"                  => "dtw"                 => "world", 0;
  df_char                  : "hello world"                  => "dfw"                 => "orld", 0;
  dh_back                  : "hello"                        => "lldh"                => "hllo", 1;
  dl_forward               : "hello"                        => "dl"                  => "ello", 0;
  dge_back_end             : "one two three"                => "$dge"                => "one tw", 5;
  dG_to_end                : "hello world"                  => "dG"                  => "", 0;
  dgg_to_start             : "hello world"                  => "$dgg"                => "", 0;
  d_semicolon              : "abcabc"                       => "fad;"                => "abcabc", 3;
  cw_basic                 : "hello world"                  => "cwfoo\x1b"           => "foo world", 2;
  C_to_end                 : "hello world"                  => "wCfoo\x1b"           => "hello foo", 8;
  cc_whole                 : "hello world"                  => "ccfoo\x1b"           => "foo", 2;
  ct_char                  : "hello world"                  => "ctwfoo\x1b"          => "fooworld", 2;
  s_single                 : "hello"                        => "sfoo\x1b"            => "fooello", 2;
  S_whole_line             : "hello world"                  => "Sfoo\x1b"            => "foo", 2;
  cl_forward               : "hello"                        => "clX\x1b"             => "Xello", 0;
  ch_backward              : "hello"                        => "llchX\x1b"           => "hXllo", 1;
  cb_word_back             : "hello world"                  => "$cbfoo\x1b"          => "hello food", 8;
  ce_word_end              : "hello world"                  => "cefoo\x1b"           => "foo world", 2;
  c0_to_start              : "hello world"                  => "wc0foo\x1b"          => "fooworld", 2;
  yw_p_basic               : "hello world"                  => "ywwP"                => "hello hello world", 11;
  dw_p_paste               : "hello world"                  => "dwP"                 => "hello world", 5;
  dd_p_paste               : "hello world"                  => "ddp"                 => "\nhello world", 1;
  y_dollar_p               : "hello world"                  => "wy$P"                => "hello worldworld", 10;
  ye_p                     : "hello world"                  => "yewP"                => "hello helloworld", 10;
  yy_p                     : "hello world"                  => "yyp"                 => "hello world\nhello world", 12;
  Y_p                      : "hello world"                  => "Yp"                  => "hhello worldello world", 11;
  p_after_x                : "hello"                        => "xp"                  => "ehllo", 1;
  P_before                 : "hello"                        => "llxP"                => "hello", 2;
  paste_empty              : "hello"                        => "p"                   => "hello", 0;
  r_replace                : "hello"                        => "ra"                  => "aello", 0;
  r_middle                 : "hello"                        => "llra"                => "healo", 2;
  r_at_end                 : "hello"                        => "$ra"                 => "hella", 4;
  r_space                  : "hello"                        => "r "                  => " ello", 0;
  r_with_count             : "hello"                        => "3rx"                 => "xxxlo", 2;
  tilde_single             : "hello"                        => "~"                   => "Hello", 1;
  tilde_count              : "hello"                        => "3~"                  => "HELlo", 3;
  tilde_at_end             : "HELLO"                        => "$~"                  => "HELLo", 4;
  tilde_mixed              : "hElLo"                        => "5~"                  => "HeLlO", 4;
  gu_word                  : "HELLO world"                  => "guw"                 => "hello world", 0;
  gU_word                  : "hello WORLD"                  => "gUw"                 => "HELLO WORLD", 0;
  gu_dollar                : "HELLO WORLD"                  => "gu$"                 => "hello world", 0;
  gU_dollar                : "hello world"                  => "gU$"                 => "HELLO WORLD", 0;
  gu_0                     : "HELLO WORLD"                  => "$gu0"                => "hello worlD", 0;
  gU_0                     : "hello world"                  => "$gU0"                => "HELLO WORLd", 0;
  gtilde_word              : "hello WORLD"                  => "g~w"                 => "HELLO WORLD", 0;
  gtilde_dollar            : "hello WORLD"                  => "g~$"                 => "HELLO world", 0;
  diw_inner                : "one two three"                => "wdiw"                => "one  three", 4;
  ciw_replace              : "hello world"                  => "ciwfoo\x1b"          => "foo world", 2;
  daw_around               : "one two three"                => "wdaw"                => "one three", 4;
  yiw_p                    : "hello world"                  => "yiwAp \x1bp"         => "hello worldp hello", 17;
  diW_big_inner            : "one-two three"                => "diW"                 => " three", 0;
  daW_big_around           : "one two-three end"            => "wdaW"                => "one end", 4;
  ciW_big                  : "one-two three"                => "ciWx\x1b"            => "x three", 0;
  di_dquote                : "one \"two\" three"            => "f\"di\""             => "one \"\" three", 5;
  da_dquote                : "one \"two\" three"            => "f\"da\""             => "one three", 4;
  ci_dquote                : "one \"two\" three"            => "f\"ci\"x\x1b"        => "one \"x\" three", 5;
  di_squote                : "one 'two' three"              => "f'di'"               => "one '' three", 5;
  da_squote                : "one 'two' three"              => "f'da'"               => "one three", 4;
  di_backtick              : "one `two` three"              => "f`di`"               => "one `` three", 5;
  da_backtick              : "one `two` three"              => "f`da`"               => "one three", 4;
  ci_dquote_empty          : "one \"\" three"               => "f\"ci\"x\x1b"        => "one \"x\" three", 5;
  di_paren                 : "one (two) three"              => "f(di("               => "one () three", 5;
  da_paren                 : "one (two) three"              => "f(da("               => "one  three", 4;
  ci_paren                 : "one (two) three"              => "f(ci(x\x1b"          => "one (x) three", 5;
  di_brace                 : "one {two} three"              => "f{di{"               => "one {} three", 5;
  da_brace                 : "one {two} three"              => "f{da{"               => "one  three", 4;
  di_bracket               : "one [two] three"              => "f[di["               => "one [] three", 5;
  da_bracket               : "one [two] three"              => "f[da["               => "one  three", 4;
  di_angle                 : "one <two> three"              => "f<di<"               => "one <> three", 5;
  da_angle                 : "one <two> three"              => "f<da<"               => "one  three", 4;
  di_paren_nested          : "fn(a, (b, c))"                => "f(di("               => "fn()", 3;
  di_paren_empty           : "fn() end"                     => "f(di("               => "fn() end", 3;
  dib_alias                : "one (two) three"              => "f(dib"               => "one () three", 5;
  diB_alias                : "one {two} three"              => "f{diB"               => "one {} three", 5;
  percent_paren            : "(hello) world"                => "%"                   => "(hello) world", 6;
  percent_brace            : "{hello} world"                => "%"                   => "{hello} world", 6;
  percent_bracket          : "[hello] world"                => "%"                   => "[hello] world", 6;
  percent_from_close       : "(hello) world"                => "f)%"                 => "(hello) world", 0;
  d_percent_paren          : "(hello) world"                => "d%"                  => " world", 0;
  to_paren_fwd             : "foo (bar) baz"                => "])"                  => "foo (bar) baz", 8;
  to_paren_bkwd            : "foo (bar) baz"                => "f)[("                => "foo (bar) baz", 4;
  to_brace_fwd             : "foo {bar} baz"                => "]}"                  => "foo {bar} baz", 8;
  to_brace_bkwd            : "foo {bar} baz"                => "f}[{"                => "foo {bar} baz", 4;
  to_paren_nested          : "((a)(b)) end"                 => "])"                  => "((a)(b)) end", 7;
  to_brace_nested          : "{{a}{b}} end"                 => "]}"                  => "{{a}{b}} end", 7;
  d_to_paren_fwd           : "foo (bar) baz"                => "wd])"                => "foo  baz", 4;
  d_to_brace_fwd           : "foo {bar} baz"                => "wd]}"                => "foo  baz", 4;
  to_paren_no_match        : "foo bar baz"                  => "])"                  => "foo bar baz", 0;
  to_brace_no_match        : "foo bar baz"                  => "]}"                  => "foo bar baz", 0;
  i_insert                 : "hello"                        => "iX\x1b"              => "Xhello", 0;
  a_append                 : "hello"                        => "aX\x1b"              => "hXello", 1;
  I_front                  : "  hello"                      => "IX\x1b"              => "  Xhello", 2;
  A_end                    : "hello"                        => "AX\x1b"              => "helloX", 5;
  o_open_below             : "hello"                        => "oworld\x1b"          => "hello\nworld", 10;
  O_open_above             : "hello"                        => "Oworld\x1b"          => "world\nhello", 4;
  empty_input              : ""                             => "i hello\x1b"         => " hello", 5;
  insert_escape            : "hello"                        => "aX\x1b"              => "hXello", 1;
  ctrl_w_del_word          : "hello world"                  => "A\x17\x1b"           => "hello ", 5;
  ctrl_h_backspace         : "hello"                        => "A\x08\x1b"           => "hell", 3;
  ctrl_o_dw                : "hello world"                  => "i\x0fdw\x1b"         => "world", 0;
  ctrl_o_x                 : "hello"                        => "i\x0fx\x1b"          => "ello", 0;
  ctrl_o_motion            : "hello world"                  => "i\x0fwX\x1b"         => "hello Xworld", 6;
  u_undo_delete            : "hello world"                  => "dwu"                 => "hello world", 0;
  u_undo_change            : "hello world"                  => "ciwfoo\x1bu"         => "hello world", 0;
  u_undo_x                 : "hello"                        => "xu"                  => "hello", 0;
  ctrl_r_redo              : "hello"                        => "xu\x12"              => "ello", 0;
  u_multiple               : "hello world"                  => "xdwu"                => "ello world", 0;
  redo_after_undo          : "hello world"                  => "dwu\x12"             => "world", 0;
  dot_repeat_x             : "hello"                        => "x."                  => "llo", 0;
  dot_repeat_dw            : "one two three"                => "dw."                 => "three", 0;
  dot_repeat_cw            : "one two three"                => "cwfoo\x1bw."         => "foo foo three", 6;
  dot_repeat_r             : "hello"                        => "ra.."                => "aello", 0;
  dot_repeat_s             : "hello"                        => "sX\x1bl."            => "XXllo", 1;
  count_h                  : "hello world"                  => "$3h"                 => "hello world", 7;
  count_l                  : "hello world"                  => "3l"                  => "hello world", 3;
  count_w                  : "one two three four"           => "2w"                  => "one two three four", 8;
  count_b                  : "one two three four"           => "$2b"                 => "one two three four", 8;
  count_x                  : "hello"                        => "3x"                  => "lo", 0;
  count_dw                 : "one two three four"           => "2dw"                 => "three four", 0;
  verb_count_motion        : "one two three four"           => "d2w"                 => "three four", 0;
  count_s                  : "hello"                        => "3sX\x1b"             => "Xlo", 0;
  indent_line              : "hello"                        => ">>"                  => "\thello", 1;
  dedent_line              : "\thello"                      => "<<"                  => "hello", 0;
  indent_double            : "hello"                        => ">>>>"                => "\t\thello", 2;
  J_join_lines             : "hello\nworld"                 => "J"                   => "hello world", 5;
  v_u_lower                : "HELLO"                        => "vlllu"               => "hellO", 0;
  v_U_upper                : "hello"                        => "vlllU"               => "HELLo", 0;
  v_d_delete               : "hello world"                  => "vwwd"                => "", 0;
  v_x_delete               : "hello world"                  => "vwwx"                => "", 0;
  v_c_change               : "hello world"                  => "vwcfoo\x1b"          => "fooorld", 2;
  v_y_p_yank               : "hello world"                  => "vwyAp \x1bp"         => "hello worldp hello w", 19;
  v_dollar_d               : "hello world"                  => "wv$d"                => "hello ", 5;
  v_0_d                    : "hello world"                  => "$v0d"                => "", 0;
  ve_d                     : "hello world"                  => "ved"                 => " world", 0;
  v_o_swap                 : "hello world"                  => "vllod"               => "lo world", 0;
  v_r_replace              : "hello"                        => "vlllrx"              => "xxxxo", 0;
  v_tilde_case             : "hello"                        => "vlll~"               => "HELLo", 0;
  V_d_delete               : "hello world"                  => "Vd"                  => "", 0;
  V_y_p                    : "hello world"                  => "Vyp"                 => "hello world\nhello world", 12;
  V_S_change               : "hello world"                  => "VSfoo\x1b"           => "foo", 2;
  ctrl_a_inc               : "num 5 end"                    => "w\x01"               => "num 6 end", 4;
  ctrl_x_dec               : "num 5 end"                    => "w\x18"               => "num 4 end", 4;
  ctrl_a_negative          : "num -3 end"                   => "w\x01"               => "num -2 end", 5;
  ctrl_x_to_neg            : "num 0 end"                    => "w\x18"               => "num -1 end", 5;
  ctrl_a_count             : "num 5 end"                    => "w3\x01"              => "num 8 end", 4;
  ctrl_a_width             : "num -00001 end"               => "w\x01"               => "num 00000 end", 8;
  delete_empty             : ""                             => "x"                   => "", 0;
  undo_on_empty            : ""                             => "u"                   => "", 0;
  w_single_char            : "a b c"                        => "w"                   => "a b c", 2;
  dw_last_word             : "hello"                        => "dw"                  => "", 0;
  dollar_single            : "h"                            => "$"                   => "h", 0;
  caret_no_ws              : "hello"                        => "$^"                  => "hello", 0;
  f_last_char              : "hello"                        => "fo"                  => "hello", 4;
  r_on_space               : "hello world"                  => "5|r-"                => "hell- world", 4;
  vw_doesnt_crash          : ""                             => "vw"                  => "", 0;
  indent_cursor_pos        : "echo foo"                     => ">>"                  => "\techo foo", 1;
  join_indent_lines        : "echo foo\n\t\techo bar"       => "J"                   => "echo foo echo bar", 8;
  cw_stays_on_line         : "echo foo\necho bar"           => "wcw"                 => "echo \necho bar", 5;
  ex_sub_simple            : "echo foo\necho bar"           => ":%s/foo/bar/\r"      => "echo bar\necho bar", 0;
  ex_global_simple         : "echo foo\necho bar\necho biz" => ":g/echo/normal!dw\r" => "foo\nbar\nbiz", 8;
  ex_sub_first_only        : "foo foo foo"                  => ":s/foo/X/\r"         => "X foo foo", 0;
  ex_sub_global_flag       : "foo foo foo"                  => ":s/foo/X/g\r"        => "X X X", 0;
  ex_sub_line_range        : "foo\nfoo\nfoo"                => ":2,3s/foo/bar/\r"    => "foo\nbar\nbar", 0;
  ex_sub_single_line       : "hello\nworld"                 => ":1s/hello/hi/\r"     => "hi\nworld", 0;
  ex_repeat_sub            : "foo\nfoo"                     => ":s/foo/bar/\rj:s\r"  => "bar\nbar", 4;
  ex_repeat_sub_all        : "foo\nfoo\nfoo"                => ":s/foo/bar/\r :%s\r" => "bar\nbar\nbar", 0;
  ex_delete_cur            : "hello\nworld"                 => ":d\r"                => "world", 0;
  ex_delete_all            : "hello\nworld"                 => ":%d\r"               => "", 0;
  ex_delete_range          : "line1\nline2\nline3"          => ":1,2d\r"             => "line3", 0;
  ex_global_delete         : "echo foo\nls\necho bar"       => ":g/echo/d\r"         => "ls", 0;
  ex_global_sub            : "foo bar\nfoo baz\nkeep"       => ":g/foo/s/foo/X/\r"   => "X bar\nX baz\nkeep", 0;
  ex_normal_range          : "hello world\nfoo bar\nbiz"    => ":1,2normal!dw\r"     => "world\nbar\nbiz", 6;
  ex_repeat_global         : "echo foo\nls\necho bar\nls2"  => ":g/echo/d\r   :g\r"  => "ls\nls2", 0;
  visual_dot_repeat        : "hello\nworld\nfoo\nbar\nbiz"  => "jVjdu2k."            => "foo\nbar\nbiz", 0;
  visual_replace           : "echo ./barbiz/baz buzz"       => "wvEdwvep"            => "echo  ./barbiz/baz", 17;
  n_char_search_f          : "foo=(bar biz bam)"            => "d2fb"                => "iz bam)", 0;
  n_char_search_t          : "foo=(bar biz bam)"            => "d2tb"                => "biz bam)", 0;
  n_char_search_bkwd_f     : "foo=(bar biz bam)"            => "$d2Fb"               => "foo=(bar )", 9;
  n_char_search_bkwd_t     : "foo=(bar biz bam)"            => "$d2Tb"               => "foo=(bar b)", 10;
  count_search_fwd         : "foo=(bar biz bam)"            => "2/b\rx"              => "foo=(bar iz bam)", 9;
  count_search_bkwd        : "foo=(bar biz bam)"            => "3?b\rx"              => "foo=(ar biz bam)", 5;
  count_n_fwd              : "foo=(bar biz bam)"            => "/b\r2nx"             => "foo=(bar biz am)", 13;
  count_n_bkwd             : "foo=(bar biz bam)"            => "/b\r2Nx"             => "foo=(bar iz bam)", 9;
  macro_record             : "foo bar biz"                  => "qacwbam\x1bwqQQ"     => "bam bam bam", 10;
  macro_double             : "foo BAR biz BAM"              => "qag~wwqqbguwwq@a@b"  => "FOO bar BIZ bam", 14;

  // ugly ones go down here
  ex_nested_global
    :  "alpha bravo\nalpha charlie\ndelta bravo\ngamma"
    => ":g/alpha/g/bravo/normal!dw\r"
    => "bravo\nalpha charlie\ndelta bravo\ngamma", 0;
}

// ===================== Vi Tests =====================

fn test_vi(initial: &str) -> (ShedLine, TestGuard) {
  Shed::shopts_mut(|o| o.set.vi = true);
  let g = TestGuard::new();
  let prompt = Prompt::default();
  let vi = ShedLine::new_no_hist(prompt).unwrap().with_initial(initial);

  (vi, g)
}

#[test]
fn vi_auto_indent() {
  let (mut vi, _g) = test_vi("");

  // Type each line and press Enter separately so auto-indent triggers
  let lines = [
    "func() {",
    "case foo in",
    "bar)",
    "while true; do",
    "echo foo \\\rbar \\\rbiz \\\rbazz\rbreak\rdone\r;;\resac\r}",
  ];

  for (i, line) in lines.iter().enumerate() {
    Shed::term_mut(|t| t.feed_bytes(line.as_bytes()));
    if i != lines.len() - 1 {
      Shed::term_mut(|t| t.feed_bytes(b"\r"));
    }
    let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
    vi.process_input(keys).unwrap();
  }

  assert_eq!(
    vi.editor.joined(),
    "func() {\n\tcase foo in\n\t\tbar)\n\t\t\twhile true; do\n\t\t\t\techo foo \\\n\t\t\t\tbar \\\n\t\t\t\tbiz \\\n\t\t\t\tbazz\n\t\t\t\tbreak\n\t\t\tdone\n\t\t;;\n\tesac\n}"
  );
}

#[test]
fn vi_auto_indent_siblings() {
  let (mut vi, _g) = test_vi("");

  // Type each line and press Enter separately so auto-indent triggers
  let lines = [
    "if foo; then",
    "echo foo",
    "elif bar; then",
    "echo biz",
    "else",
    "echo bar",
    "fi",
  ];

  for (i, line) in lines.iter().enumerate() {
    Shed::term_mut(|t| t.feed_bytes(line.as_bytes()));
    if i != lines.len() - 1 {
      Shed::term_mut(|t| t.feed_bytes(b"\r"));
    }
    let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
    vi.process_input(keys).unwrap();
  }

  assert_eq!(
    vi.editor.joined(),
    "if foo; then\n\techo foo\nelif bar; then\n\techo biz\nelse\n\techo bar\nfi"
  );
}

// this one cant go up there because '\x1bO' looks like an OSC sequence to the vte parser.
// So we have to explicitly split up the key events here into 'key!(Esc)' and 'key!('O')'.
#[test]
fn vi_auto_indent_funcdef() {
  let (mut vi, _g) = test_vi("");

  let bytes = b"func_def() {}";
  Shed::term_mut(|t| t.feed_bytes(bytes));
  let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
  vi.process_input(keys).unwrap();
  vi.process_input(vec![key!(Esc)]).unwrap();
  vi.process_input(vec![key!('i')]).unwrap();
  vi.process_input(vec![key!(Enter)]).unwrap();
  vi.process_input(vec![key!(Esc)]).unwrap();
  vi.process_input(vec![key!('O')]).unwrap();
  assert_eq!(vi.editor.joined(), "func_def() {\n\t\n}");
}

#[test]
fn vi_func_def_is_finished() {
  let (mut vi, _g) = test_vi("");

  let bytes = b"func_def() {\r}\r";
  Shed::term_mut(|t| t.feed_bytes(bytes));
  let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
  vi.process_input(keys).unwrap();
  assert_eq!(vi.editor.joined(), "");
}

#[test]
fn case_stmt_is_finished() {
  let (mut vi, _g) = test_vi("");

  let bytes = b"case foo in\rfoo)\rcase bar in\rbar)\recho foo\r;;\resac\r;;\resac\r";
  Shed::term_mut(|t| t.feed_bytes(bytes));
  let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
  vi.process_input(keys).unwrap();
  assert_eq!(vi.editor.joined(), "");
}

macro_rules! hist_expansion_test {
  ($($name:ident: $cmds:expr => $input:expr => $expected:literal);* $(;)?
   ---
   $($name2:ident: $cmds2:expr => $input2:literal);* $(;)?
  ) => {
    mod hist_expansion {
      use super::*;
      $(#[test]
        fn $name() {
          let _g = TestGuard::new();
          let prompt = Prompt::default();
          let mut line = ShedLine::new_no_hist(prompt).unwrap();
          for cmd in $cmds {
            line.history.push(cmd.to_string()).unwrap();
          }
          line.history.refresh_hist_entries();
          line.history.update_search_mask(None);

          assert_eq!(line.history.masked_entries().len(), $cmds.len());

          Shed::term_mut(|t| t.feed_bytes($input.as_bytes()));
          let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
          line.process_input(keys).unwrap();

          let joined = line.editor.joined();
          assert_eq!(joined, $expected);
        }
      )*

      $(#[test]
        fn $name2() {
          let _g = TestGuard::new();
          let prompt = Prompt::default();
          let mut line = ShedLine::new_no_hist(prompt).unwrap();
          for cmd in $cmds2 {
            line.history.push(cmd.to_string()).unwrap();
          }
          line.history.update_search_mask(None);

          // Feed input without pressing Enter
          Shed::term_mut(|t| t.feed_bytes($input2.as_bytes()));
          let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
          line.process_input(keys).unwrap();

          let before = line.editor.joined();
          // Manually call attempt_history_expansion - should return false
          let expanded = line.editor.attempt_history_expansion(&line.history);
          assert!(!expanded, "expected no expansion but expansion occurred");
          assert_eq!(line.editor.joined(), before);
        }
      )*
    }
  };
}

hist_expansion_test! {
  breaks_on_metachars1   : &["cargo run"]                      => "if !car;\r"          => "if cargo run;";
  breaks_on_metachars2   : &["cargo run"]                      => "!car && true\r"      => "cargo run && true";
  breaks_on_metachars3   : &["cargo run"]                      => "!car | grep foo\r"   => "cargo run | grep foo";
  ignores_closing_quote  : &["cargo run"]                      => "echo \"foo !car\"\r" => "echo \"foo cargo run\"";
  works_in_double_quotes : &["foo"]                            => "\"!!\"\r"            => "\"foo\"";
  no_match_is_passthrough: &["foo", "bar"]                     => "!z\r"                => "z";
  multiple               : &["hello", "world"]                 => "echo !! !h\r"        => "echo world hello";
  inline                 : &["world"]                          => "echo !!\r"           => "echo world";
  positive_index         : &["alpha", "beta", "gamma"]         => "!1\r"                => "alpha";
  negative_index         : &["alpha", "beta", "gamma"]         => "!-2\r"               => "beta";
  bang_dollar_single_word: &["solo"]                           => "!$\r"                => "solo";
  bang_dollar            : &["echo hello world"]               => "!$\r"                => "world";
  bang_bang              : &["echo hello", "ls"]               => "!!\r"                => "ls";
  prefix_latest_match    : &["foo first", "bar", "foo second"] => "!f\r"                => "foo second";
  prefix                 : &["foo", "bar", "biz", "qux"]       => "!f\r"                => "foo";

  multiline
    : &["echo foo","if true; then\necho foo\nfi", "echo bar"]
    => "!2\r"
    => "if true; then\n\techo foo\nfi";

  parse_recurses
    : &["cargo run"]
    => "echo \"foo $(echo \"!car\") bar\"\r"
    => "echo \"foo $(echo \"cargo run\") bar\"";

  --- // no-expansion tests
  skips_param_indirection1    : &["cargo run"] => "echo ${!var}";
  skips_param_indirection2    : &["cargo run"] => "echo ${!car}";
  skips_param_indirection3    : &["cargo run"] => "echo \"${!var}\"";
  no_recurse_in_single_quotes2: &["cargo run"] => "echo 'foo $(echo \"!car\") bar'";
  no_recurse_in_single_quotes1: &["cargo run"] => "echo \"foo $(echo '!car') bar\"\r";
  skips_dollar_bang           : &["foo"]       => "$!1";
  skips_single_quotes         : &["foo"]       => "'!!'";
}

// ===================== History General Tests =====================

use crate::readline::history::History;

#[test]
fn hist_push_returns_id() {
  let _g = TestGuard::new();
  let hist = History::empty("test_push_id");
  let id1 = hist.push("cmd1".into()).unwrap();
  let id2 = hist.push("cmd2".into()).unwrap();
  assert!(id1.is_some());
  assert!(id2.is_some());
  assert_ne!(id1, id2);
}

#[test]
fn hist_push_empty_returns_none() {
  let _g = TestGuard::new();
  let hist = History::empty("test_push_empty");
  let id = hist.push("".into()).unwrap();
  assert!(id.is_none());
  assert_eq!(hist.entry_count(), 0);
}

#[test]
fn hist_entry_count() {
  let _g = TestGuard::new();
  let hist = History::empty("test_count");
  assert_eq!(hist.entry_count(), 0);
  hist.push("cmd1".into()).unwrap();
  assert_eq!(hist.entry_count(), 1);
  hist.push("cmd2".into()).unwrap();
  hist.push("cmd3".into()).unwrap();
  assert_eq!(hist.entry_count(), 3);
}

#[test]
fn hist_last_id() {
  let _g = TestGuard::new();
  let hist = History::empty("test_last_id");
  assert_eq!(hist.last_id(), 0);
  hist.push("cmd1".into()).unwrap();
  assert_eq!(hist.last_id(), 1);
  hist.push("cmd2".into()).unwrap();
  assert_eq!(hist.last_id(), 2);
}

#[test]
fn hist_last_returns_most_recent() {
  let _g = TestGuard::new();
  let hist = History::empty("test_last");
  assert!(hist.last().is_none());
  hist.push("first".into()).unwrap();
  hist.push("second".into()).unwrap();
  let last = hist.last().unwrap();
  assert_eq!(last.command, "second");
}

#[test]
fn hist_query_with_filter() {
  let _g = TestGuard::new();
  let hist = History::empty("test_query_filter");
  hist.push("echo foo".into()).unwrap();
  hist.push("ls -la".into()).unwrap();
  hist.push("echo bar".into()).unwrap();

  let results = hist
    .query(
      "WHERE command LIKE ?1 ORDER BY id ASC",
      &[&"echo%" as &dyn rusqlite::ToSql],
    )
    .unwrap();
  assert_eq!(results.len(), 2);
  assert_eq!(results[0].1.command, "echo foo");
  assert_eq!(results[1].1.command, "echo bar");
}

#[test]
fn hist_query_range() {
  let _g = TestGuard::new();
  let hist = History::empty("test_query_range");
  hist.push("cmd1".into()).unwrap();
  hist.push("cmd2".into()).unwrap();
  hist.push("cmd3".into()).unwrap();
  hist.push("cmd4".into()).unwrap();

  let results = hist.query_range(2, 3).unwrap();
  assert_eq!(results.len(), 2);
  assert_eq!(results[0].1.command, "cmd2");
  assert_eq!(results[1].1.command, "cmd3");
}

#[test]
fn hist_ids_are_sequential() {
  let _g = TestGuard::new();
  let hist = History::empty("test_sequential");
  hist.push("a".into()).unwrap();
  hist.push("b".into()).unwrap();
  hist.push("c".into()).unwrap();

  let entries = hist.query("ORDER BY id ASC", &[]).unwrap();
  for (i, (id, _)) in entries.iter().enumerate() {
    assert_eq!(*id, (i + 1) as i64);
  }
}

// ===================== History Delete/Restore Tests =====================

#[test]
fn hist_delete_removes_entries() {
  let _g = TestGuard::new();
  let hist = History::empty("test_delete");
  hist.push("echo foo".into()).unwrap();
  hist.push("echo bar".into()).unwrap();
  hist.push("echo baz".into()).unwrap();
  assert_eq!(hist.entry_count(), 3);

  let deleted = hist
    .delete("WHERE command = ?1", &[&"echo bar" as &dyn rusqlite::ToSql])
    .unwrap();
  assert_eq!(deleted.len(), 1);
  assert_eq!(deleted[0].1.command, "echo bar");
  assert_eq!(hist.entry_count(), 2);
}

#[test]
fn hist_delete_reids_contiguously() {
  let _g = TestGuard::new();
  let hist = History::empty("test_reid");
  hist.push("cmd1".into()).unwrap();
  hist.push("cmd2".into()).unwrap();
  hist.push("cmd3".into()).unwrap();

  hist
    .delete("WHERE command = ?1", &[&"cmd2" as &dyn rusqlite::ToSql])
    .unwrap();

  let entries = hist.query("ORDER BY id ASC", &[]).unwrap();
  assert_eq!(entries.len(), 2);
  assert_eq!(entries[0].0, 1); // id should be 1
  assert_eq!(entries[1].0, 2); // id should be 2, not 3
}

#[test]
fn hist_delete_creates_backup() {
  let _g = TestGuard::new();
  let hist = History::empty("test_backup");
  hist.push("echo foo".into()).unwrap();
  hist.push("echo bar".into()).unwrap();

  hist
    .delete("WHERE command = ?1", &[&"echo bar" as &dyn rusqlite::ToSql])
    .unwrap();

  // backup should exist - restore should succeed, not error
  assert!(hist.restore_backup().is_ok());
}

#[test]
fn hist_restore_recovers_deleted_entries() {
  let _g = TestGuard::new();
  let hist = History::empty("test_restore");
  hist.push("echo foo".into()).unwrap();
  hist.push("echo bar".into()).unwrap();
  hist.push("echo baz".into()).unwrap();

  hist
    .delete("WHERE command = ?1", &[&"echo bar" as &dyn rusqlite::ToSql])
    .unwrap();

  assert_eq!(hist.entry_count(), 2);

  let restored = hist.restore_backup().unwrap();
  assert_eq!(restored, 1);
  assert_eq!(hist.entry_count(), 3);
}

#[test]
fn hist_restore_preserves_new_entries() {
  let _g = TestGuard::new();
  let hist = History::empty("test_restore_new");
  hist.push("cmd1".into()).unwrap();
  hist.push("cmd2".into()).unwrap();
  hist.push("cmd3".into()).unwrap();

  hist
    .delete("WHERE command = ?1", &[&"cmd2" as &dyn rusqlite::ToSql])
    .unwrap();

  // add a new command after the delete
  hist.push("cmd4".into()).unwrap();

  let restored = hist.restore_backup().unwrap();
  assert_eq!(restored, 1); // only cmd2 was missing
  assert_eq!(hist.entry_count(), 4);

  // all commands should be present, ordered by timestamp
  let entries = hist.query("ORDER BY id ASC", &[]).unwrap();
  let cmds: Vec<&str> = entries.iter().map(|(_, e)| e.command.as_str()).collect();
  assert!(cmds.contains(&"cmd1"));
  assert!(cmds.contains(&"cmd2"));
  assert!(cmds.contains(&"cmd3"));
  assert!(cmds.contains(&"cmd4"));
}

#[test]
fn hist_restore_no_backup_errors() {
  let _g = TestGuard::new();
  let hist = History::empty("test_no_backup");
  hist.push("echo foo".into()).unwrap();

  assert!(hist.restore_backup().is_err());
}

#[test]
fn hist_restore_ids_are_contiguous() {
  let _g = TestGuard::new();
  let hist = History::empty("test_restore_ids");
  hist.push("cmd1".into()).unwrap();
  hist.push("cmd2".into()).unwrap();
  hist.push("cmd3".into()).unwrap();

  hist
    .delete("WHERE command = ?1", &[&"cmd2" as &dyn rusqlite::ToSql])
    .unwrap();
  hist.push("cmd4".into()).unwrap();
  hist.restore_backup().unwrap();

  let entries = hist.query("ORDER BY id ASC", &[]).unwrap();
  for (i, (id, _)) in entries.iter().enumerate() {
    assert_eq!(*id, (i + 1) as i64, "IDs should be contiguous");
  }
}

// ===================== Alias Expansion Tests =====================

macro_rules! alias_expansion_test {
  ($($name:ident: $aliases:expr => $input:literal => $result:literal);* $(;)?
   ---
   $($name2:ident: $aliases2:expr => $input2:literal);* $(;)?
  ) => {
    mod alias {
      use super::*;
      $(#[test]
        fn $name() {
          let _g = TestGuard::new();
          Shed::shopts_mut(|o| o.prompt.expand_aliases = true);
          Shed::logic_mut(|l| {
            for (name, body) in $aliases {
              l.insert_alias(name, body, Span::default());
            }
          });

          let prompt = Prompt::default();
          let mut line = ShedLine::new_no_hist(prompt).unwrap();

          Shed::term_mut(|t| t.feed_bytes($input.as_bytes()));
          let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
          line.process_input(keys).unwrap();

          let joined = line.editor.joined();
          assert_eq!(joined, $result, "\nInput: {:?}", $input);
        }
      )*
        $(#[test]
          fn $name2() {
            let _g = TestGuard::new();
            Shed::shopts_mut(|o| o.prompt.expand_aliases = true);
            Shed::logic_mut(|l| {
              for (name, body) in $aliases2 {
                l.insert_alias(name, body, Span::default());
              }
            });

            let prompt = Prompt::default();
            let mut line = ShedLine::new_no_hist(prompt).unwrap();

            Shed::term_mut(|t| t.feed_bytes($input2.as_bytes()));
            let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
            line.process_input(keys).unwrap();

            let before = line.editor.joined();
            let expanded = line.editor.attempt_alias_expansion();
            assert!(
              !expanded,
              "expected no alias expansion but expansion occurred"
            );
            assert_eq!(line.editor.joined(), before);
          }
      )*
    }
  };
}

alias_expansion_test! {
  single_char_body      : &[("a", "b")]            => "a "            => "b ";
  single_char_name      : &[("g", "git")]          => "g "            => "git ";
  expand_after_semicolon: &[("gc", "git commit")]  => "echo hi; gc "  => "echo hi; git commit ";
  expansion_with_args   : &[("gc", "git commit")]  => "gc -m 'hello'" => "git commit -m 'hello'";
  simple_expansion      : &[("ll", "ls -la")]      => "ll "           => "ls -la ";

  recursion_terminates
    : &[("diff", "diff --color=auto")]
    => "diff "
    => "diff --color=auto ";

  multiple_on_same_line
    : &[("gc", "git commit"), ("gp", "git push")]
    => "gc; gp "
    => "git commit; git push ";

  ---
  no_expand_in_quotes: &[("gc", "git commit")] => "echo 'gc' ";
  no_expand_in_arg_position: &[("foo", "bar")] => "echo foo ";
}

#[test]
fn alias_no_expand_when_disabled() {
  let _g = TestGuard::new();
  Shed::shopts_mut(|o| o.prompt.expand_aliases = false);
  Shed::logic_mut(|l| {
    l.insert_alias("gc", "git commit", Span::default());
  });

  let prompt = Prompt::default();
  let mut line = ShedLine::new_no_hist(prompt).unwrap();

  Shed::term_mut(|t| t.feed_bytes(b"gc "));
  let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
  line.process_input(keys).unwrap();

  let joined = line.editor.joined();
  assert_ne!(joined, "git commit");
}

// ===================== Hint Tests =====================
//
// prepopulates some commands into the test harness command history
// then does line editor stuff that would interact with the hint system
macro_rules! hint_test {
  { $($name:ident
      : $hist:expr
      => $input:expr
      => $expected_buf:expr, $expected_hint:expr, $expected_cursor:expr
    );* $(;)?
  } => {
    mod hint {
      use super::*;
      $(#[test]
        fn $name() {
          Shed::shopts_mut(|o| {
            o.set.vi = true;
            o.line.auto_suggest = true;
          });
          let (mut line, _g) = test_vi("");

          for cmd in $hist {
            line.history.push(cmd.to_string()).unwrap();
          }
          line.history.refresh_hist_entries();
          line.history.constrain_entries(None);

          for byte in $input.as_bytes() {
            Shed::term_mut(|t| t.feed_bytes(&[*byte]));
            let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
            line.process_input(keys).unwrap();
          }

          assert_eq!(line.editor.joined(), $expected_buf, "buffer mismatch");
          assert_eq!(
            line.editor.try_join_hint().unwrap_or_default(),
            $expected_hint,
            "hint mismatch"
          );
          assert_eq!(line.editor.cursor_to_flat(), $expected_cursor, "cursor mismatch");
        }
      )*
    }
  };
}

hint_test! {
  prefix_suggests_hint
    : &["echo foo bar"]
    => "echo"
    => "echo", "echo foo bar", 4;

  full_accept_by_l
    : &["echo foo bar"]
    => "echo\x1bl"
    => "echo foo bar", "", 11;

  word_accept
    : &["echo foo bar"]
    => "echo\x1bw"
    => "echo f", "echo foo bar", 5;

  escape_preserves_buffer
    : &["echo foo bar"]
    => "echo\x1b"
    => "echo", "echo foo bar", 3;

  word_through_brace
    : &["flog -p \"[%H:%M:%S {level}\" info foo"]
    => "flog -p \"[%H:%M:%S {\x1bww"
    => "flog -p \"[%H:%M:%S {level}", "flog -p \"[%H:%M:%S {level}\" info foo", 25;

  word_across_line_boundary
    : &["echo foo\necho bar"]
    => "echo foo\x1bww"
    => "echo foo\necho b", "echo foo\necho bar", 14;

  back_motion_no_accept
    : &["echo foo bar"]
    => "echo\x1bb"
    => "echo", "echo foo bar", 0;

  j_accepts_downward
    : &["echo foo\necho bar\necho biz"]
    => "echo f\x1bj"
    => "echo foo\necho b", "echo foo\necho bar\necho biz", 14;

  j_accepts_downward_insert_mode_inclusive
    : &["echo foo\necho bar\necho biz"]
    => "echo \x0fj" // ctrl+o, j
    => "echo foo\necho ", "echo foo\necho bar\necho biz", 14;

  hint_constrained
    : &[
      "echo foobar",
      "echo foo",
      "echo foooooo"
    ]
    => "echo foob\x1be"
    => "echo foobar", "", 10;

  most_recent_wins
    : &[
      "echo apple",
      "echo banana",
      "echo cherry",
    ]
    => "echo "
    => "echo ", "echo cherry", 5;

  prefix_narrowing
    : &[
      "echo apple",
      "echo banana",
    ]
    => "echo a"
    => "echo a", "echo apple", 6;

  divergence_clears_hint
    : &[
      "echo foo bar",
    ]
    => "echo z"
    => "echo z", "", 6;

  exact_buffer_no_hint
    : &[
      "echo foo",
    ]
    => "echo foo"
    => "echo foo", "", 8;

  no_match_no_hint
    : &[
      "echo foo",
      "ls -la",
    ]
    => "git status"
    => "git status", "", 10;

  backspace_revives_hint
    : &[
      "echo foobar",
    ]
    => "echo z\x7f"
    => "echo ", "echo foobar", 5;
}
