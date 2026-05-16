
use pretty_assertions::assert_eq;

use super::{NdRule, node::NdKind};
use crate::tests::testutil::get_ast;

#[test]
fn parse_hello_world() {
  let input = "echo hello world";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_if_statement() {
  let input = "if echo foo; then echo bar; fi";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::IfNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_pipeline() {
  let input = "ls | grep foo | wc -l";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Command,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_conjunction_and() {
  let input = "echo foo && echo bar";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_while_loop() {
  let input = "while true; do echo hello; done";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::LoopNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_for_loop() {
  let input = "for i in a b c; do echo $i; done";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::ForNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_case_statement() {
  let input = "case foo in bar) echo bar;; baz) echo baz;; esac";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::CaseNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_func_def() {
  let input = "foo() { echo hello; }";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::FuncDef,
    NdKind::BraceGrp,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_assignment() {
  let input = "FOO=bar";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Assignment,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_assignment_with_command() {
  let input = "FOO=bar echo hello";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Assignment,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_if_elif_else() {
  let input = "if true; then echo a; elif false; then echo b; else echo c; fi";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::IfNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_brace_group() {
  let input = "{ echo hello; echo world; }";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::BraceGrp,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_nested_if_in_while() {
  let input = "while true; do if false; then echo no; fi; done";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::LoopNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::IfNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_test_bracket() {
  let input = "[[ -n hello ]]";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Test,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_nested_func_with_if_and_loop() {
  let input = "setup() {
			for f in a b c; do
				if [[ -n $f ]]; then
					echo $f
				fi
			done
		}";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::FuncDef,
    NdKind::BraceGrp,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::ForNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::IfNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Test,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_pipeline_with_brace_groups() {
  let input = "{ echo foo; echo bar; } | { grep foo; wc -l; }";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::BraceGrp,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::BraceGrp,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_deeply_nested_if() {
  let input = "if true; then
			if false; then
				if true; then
					echo deep
				fi
			fi
		fi";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::IfNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::IfNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::IfNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_case_with_multiple_commands() {
  let input = "case $1 in
			start)
				echo starting
				run_server
			;;
			stop)
				echo stopping
				kill_server
			;;
			*)
				echo unknown
			;;
		esac";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::CaseNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_func_with_case_and_conjunction() {
  let input = "dispatch() {
			case $1 in
				build)
					make clean && make all
				;;
				test)
					make test || echo failed
				;;
			esac
		}";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::FuncDef,
    NdKind::BraceGrp,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::CaseNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_while_with_pipeline_and_assignment() {
  let input = "while read line; do
			FOO=bar echo $line | grep pattern | wc -l
		done";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::LoopNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Assignment,
    NdKind::Command,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_nested_loops() {
  let input = "for i in 1 2 3; do
			for j in a b c; do
				while true; do
					echo $i $j
				done
			done
		done";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::ForNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::ForNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::LoopNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_complex_conjunction_chain() {
  let input = "mkdir -p dir && cd dir && touch file || echo failed && echo done";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_func_defining_inner_func() {
  let input = "outer() {
			inner() {
				echo hello from inner
			}
			inner
		}";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::FuncDef,
    NdKind::BraceGrp,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::FuncDef,
    NdKind::BraceGrp,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_multiline_if_elif_with_pipelines() {
  let input = "if cat /etc/passwd | grep root; then
			echo found root
		elif ls /tmp | wc -l; then
			echo tmp has files
		else
			echo fallback | tee log.txt
		fi";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::IfNode,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_cursed_input() {
  // valid shell syntax btw
  // your editor might not enjoy this
  let input = "if if while if if until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; else while :; do :; done; fi; then if until :; do :; done; then until :; do :; done; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; else while :; do :; done; fi; elif while while :; do :; done; do until :; do :; done; done; then while until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; elif until case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; then until if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; else case foo in; foo) while :; do :; done;; bar) until :; do :; done;; biz) until :; do :; done;; esac; fi; do while case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) case foo in; foo) :;; bar) :;; biz) :;; esac;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; done; then until while if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; do until while :; do :; done; do until :; do :; done; done; done; elif until until until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; do case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) if :; then :; elif :; then :; elif :; then :; else :; fi;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; done; then case foo in; foo) case foo in; foo) while :; do :; done;; bar) while :; do :; done;; biz) until :; do :; done;; esac;; bar) if until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; else while :; do :; done; fi;; biz) if until :; do :; done; then until :; do :; done; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; else while :; do :; done; fi;; esac; elif case foo in; foo) while while :; do :; done; do until :; do :; done; done;; bar) while until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done;; biz) until case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done;; esac; then if until if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; then case foo in; foo) while :; do :; done;; bar) until :; do :; done;; biz) until :; do :; done;; esac; elif case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) case foo in; foo) :;; bar) :;; biz) :;; esac;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; then if if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; elif while :; do :; done; then until :; do :; done; elif until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; elif if if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif while :; do :; done; then while :; do :; done; elif until :; do :; done; then until :; do :; done; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; then while case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; else while if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; fi; else if until while :; do :; done; do until :; do :; done; done; then until until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; elif case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) if :; then :; elif :; then :; elif :; then :; else :; fi;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; then case foo in; foo) while :; do :; done;; bar) while :; do :; done;; biz) until :; do :; done;; esac; elif if until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; else while :; do :; done; fi; then if until :; do :; done; then until :; do :; done; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; else while :; do :; done; fi; else while while :; do :; done; do until :; do :; done; done; fi; fi; then while while while until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; do until case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; done; do while until if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; do case foo in; foo) while :; do :; done;; bar) until :; do :; done;; biz) until :; do :; done;; esac; done; done; elif until until case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) case foo in; foo) :;; bar) :;; biz) :;; esac;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; do if if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; elif while :; do :; done; then until :; do :; done; elif until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; done; do until if if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif while :; do :; done; then while :; do :; done; elif until :; do :; done; then until :; do :; done; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; do while case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; done; done; then case foo in; foo) case foo in; foo) while if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done;; bar) until while :; do :; done; do until :; do :; done; done;; biz) until until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done;; esac;; bar) case foo in; foo) case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) if :; then :; elif :; then :; elif :; then :; else :; fi;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac;; bar) case foo in; foo) while :; do :; done;; bar) while :; do :; done;; biz) until :; do :; done;; esac;; biz) if until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; else while :; do :; done; fi;; esac;; biz) if if until :; do :; done; then until :; do :; done; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; else while :; do :; done; fi; then while while :; do :; done; do until :; do :; done; done; elif while until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; then until case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; elif until if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; then case foo in; foo) while :; do :; done;; bar) until :; do :; done;; biz) until :; do :; done;; esac; else case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) case foo in; foo) :;; bar) :;; biz) :;; esac;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; fi;; esac; elif if if if if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; elif while :; do :; done; then until :; do :; done; elif until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; then if if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif while :; do :; done; then while :; do :; done; elif until :; do :; done; then until :; do :; done; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; elif while case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; then while if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; elif until while :; do :; done; do until :; do :; done; done; then until until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; else case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) if :; then :; elif :; then :; elif :; then :; else :; fi;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; fi; then while case foo in; foo) while :; do :; done;; bar) while :; do :; done;; biz) until :; do :; done;; esac; do if until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; else while :; do :; done; fi; done; elif while if until :; do :; done; then until :; do :; done; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; else while :; do :; done; fi; do while while :; do :; done; do until :; do :; done; done; done; then until while until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; do until case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; done; elif until until if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; do case foo in; foo) while :; do :; done;; bar) until :; do :; done;; biz) until :; do :; done;; esac; done; then case foo in; foo) case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) case foo in; foo) :;; bar) :;; biz) :;; esac;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac;; bar) if if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; elif while :; do :; done; then until :; do :; done; elif until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi;; biz) if if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif while :; do :; done; then while :; do :; done; elif until :; do :; done; then until :; do :; done; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi;; esac; else case foo in; foo) while case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done;; bar) while if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done;; biz) until while :; do :; done; do until :; do :; done; done;; esac; fi; then if if until until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; then case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) if :; then :; elif :; then :; elif :; then :; else :; fi;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; elif case foo in; foo) while :; do :; done;; bar) while :; do :; done;; biz) until :; do :; done;; esac; then if until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; else while :; do :; done; fi; elif if until :; do :; done; then until :; do :; done; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; else while :; do :; done; fi; then while while :; do :; done; do until :; do :; done; done; else while until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; fi; then if until case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; then until if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; elif case foo in; foo) while :; do :; done;; bar) until :; do :; done;; biz) until :; do :; done;; esac; then case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) case foo in; foo) :;; bar) :;; biz) :;; esac;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; elif if if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; elif while :; do :; done; then until :; do :; done; elif until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; then if if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif while :; do :; done; then while :; do :; done; elif until :; do :; done; then until :; do :; done; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; else while case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; fi; elif while while if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; do until while :; do :; done; do until :; do :; done; done; done; then while until until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; do case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) if :; then :; elif :; then :; elif :; then :; else :; fi;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; done; elif until case foo in; foo) while :; do :; done;; bar) while :; do :; done;; biz) until :; do :; done;; esac; do if until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; else while :; do :; done; fi; done; then until if until :; do :; done; then until :; do :; done; elif case foo in; foo) :;; bar) :;; biz) :;; esac; then case foo in; foo) :;; bar) :;; biz) :;; esac; elif if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; else while :; do :; done; fi; do while while :; do :; done; do until :; do :; done; done; done; else case foo in; foo) while until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done;; bar) until case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done;; biz) until if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done;; esac; fi; else while case foo in; foo) case foo in; foo) while :; do :; done;; bar) until :; do :; done;; biz) until :; do :; done;; esac;; bar) case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) case foo in; foo) :;; bar) :;; biz) :;; esac;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac;; biz) if if :; then :; elif :; then :; elif :; then :; else :; fi; then while :; do :; done; elif while :; do :; done; then until :; do :; done; elif until :; do :; done; then case foo in; foo) :;; bar) :;; biz) :;; esac; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi;; esac; do if if if :; then :; elif :; then :; elif :; then :; else :; fi; then if :; then :; elif :; then :; elif :; then :; else :; fi; elif while :; do :; done; then while :; do :; done; elif until :; do :; done; then until :; do :; done; else case foo in; foo) :;; bar) :;; biz) :;; esac; fi; then while case foo in; foo) :;; bar) :;; biz) :;; esac; do if :; then :; elif :; then :; elif :; then :; else :; fi; done; elif while if :; then :; elif :; then :; elif :; then :; else :; fi; do while :; do :; done; done; then until while :; do :; done; do until :; do :; done; done; elif until until :; do :; done; do case foo in; foo) :;; bar) :;; biz) :;; esac; done; then case foo in; foo) case foo in; foo) :;; bar) :;; biz) :;; esac;; bar) if :; then :; elif :; then :; elif :; then :; else :; fi;; biz) if :; then :; elif :; then :; elif :; then :; else :; fi;; esac; else case foo in; foo) while :; do :; done;; bar) while :; do :; done;; biz) until :; do :; done;; esac; fi; done; fi";
  assert!(get_ast(input).is_ok()); // lets spare our sanity and just say that "ok" means "it parsed correctly"
}
#[test]
fn parse_stray_keyword_in_brace_group() {
  let input = "{ echo bar case foo in bar) echo fizz ;; buzz) echo buzz ;; esac }";
  assert!(get_ast(input).is_err());
}

// ===================== Heredocs =====================

#[test]
fn parse_basic_heredoc() {
  let input = "cat <<EOF\nhello world\nEOF";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_heredoc_with_tab_strip() {
  let input = "cat <<-EOF\n\t\thello\n\t\tworld\nEOF";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_literal_heredoc() {
  let input = "cat <<'EOF'\nhello $world\nEOF";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_herestring() {
  let input = "cat <<< \"hello world\"";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_heredoc_in_pipeline() {
  let input = "cat <<EOF | grep hello\nhello world\ngoodbye world\nEOF";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_heredoc_in_conjunction() {
  let input = "cat <<EOF && echo done\nhello\nEOF";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_heredoc_double_quoted_delimiter() {
  let input = "cat <<\"EOF\"\nhello $world\nEOF";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_heredoc_empty_body() {
  let input = "cat <<EOF\nEOF";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_heredoc_multiword_delimiter() {
  // delimiter should only be the first word
  let input = "cat <<DELIM\nsome content\nDELIM";
  let expected = &mut [
    NdKind::List,
    NdKind::Conjunction,
    NdKind::Pipeline,
    NdKind::Command,
  ]
  .into_iter();
  let ast = get_ast(input).unwrap();
  let mut node = ast[0].clone();
  if let Err(e) = node.assert_structure(expected) {
    panic!("{}", e);
  }
}

#[test]
fn parse_two_heredocs_on_one_line() {
  let input = "cat <<A; cat <<B\nfoo\nA\nbar\nB";
  let ast = get_ast(input).unwrap();
  assert_eq!(ast.len(), 1);
  let NdRule::List { ref commands } = ast[0].class else {
    panic!(
      "expected top-level List, got {:?}",
      ast[0].class.as_nd_kind()
    );
  };
  assert_eq!(commands.len(), 2);
}

// ===================== Heredoc Execution =====================

use crate::state::{Shed, vars::VarFlags, vars::VarKind};
use crate::tests::testutil::{TestGuard, test_input};

#[test]
fn heredoc_basic_output() {
  let guard = TestGuard::new();
  test_input("cat <<EOF\nhello world\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello world\n");
}

#[test]
fn heredoc_multiline_output() {
  let guard = TestGuard::new();
  test_input("cat <<EOF\nline one\nline two\nline three\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "line one\nline two\nline three\n");
}

#[test]
fn heredoc_variable_expansion() {
  let guard = TestGuard::new();
  Shed::vars_mut(|v| v.set_var("NAME", VarKind::Str("world".into()), VarFlags::empty())).unwrap();
  test_input("cat <<EOF\nhello $NAME\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello world\n");
}

#[test]
fn heredoc_literal_no_expansion() {
  let guard = TestGuard::new();
  Shed::vars_mut(|v| v.set_var("NAME", VarKind::Str("world".into()), VarFlags::empty())).unwrap();
  test_input("cat <<'EOF'\nhello $NAME\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello $NAME\n");
}

#[test]
fn heredoc_tab_stripping() {
  let guard = TestGuard::new();
  test_input("cat <<-EOF\n\t\thello\n\t\tworld\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello\nworld\n");
}

#[test]
fn heredoc_tab_stripping_uneven() {
  let guard = TestGuard::new();
  test_input("cat <<-EOF\n\t\t\thello\n\tworld\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello\nworld\n");
}

#[test]
fn heredoc_empty_body() {
  let guard = TestGuard::new();
  test_input("cat <<EOF\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "");
}

#[test]
fn heredoc_in_pipeline() {
  let guard = TestGuard::new();
  test_input("cat <<EOF | grep hello\nhello world\ngoodbye world\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello world\n");
}

#[test]
fn herestring_basic() {
  let guard = TestGuard::new();
  test_input("cat <<< \"hello world\"".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello world\n");
}

#[test]
fn herestring_variable_expansion() {
  let guard = TestGuard::new();
  Shed::vars_mut(|v| v.set_var("MSG", VarKind::Str("hi there".into()), VarFlags::empty())).unwrap();
  test_input("cat <<< $MSG".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hi there\n");
}

#[test]
fn heredoc_double_quoted_delimiter_is_literal() {
  let guard = TestGuard::new();
  Shed::vars_mut(|v| v.set_var("X", VarKind::Str("val".into()), VarFlags::empty())).unwrap();
  test_input("cat <<\"EOF\"\nhello $X\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello $X\n");
}

#[test]
fn heredoc_preserves_blank_lines() {
  let guard = TestGuard::new();
  test_input("cat <<EOF\nfirst\n\nsecond\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "first\n\nsecond\n");
}

#[test]
fn heredoc_tab_strip_preserves_empty_lines() {
  let guard = TestGuard::new();
  test_input("cat <<-EOF\n\thello\n\n\tworld\nEOF".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "hello\n\nworld\n");
}

#[test]
fn heredoc_two_on_one_line() {
  let guard = TestGuard::new();
  test_input("cat <<A; cat <<B\nfoo\nA\nbar\nB".to_string()).unwrap();
  let out = guard.read_output();
  assert_eq!(out, "foo\nbar\n");
}
