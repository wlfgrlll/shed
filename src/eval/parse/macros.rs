/// Try to match a specific parsing rule
///
/// # Notes
/// * If the match fails, execution continues.
/// * If the match succeeds, the matched node is returned.
macro_rules! try_match {
  ($expr:expr) => {
    if let Some(node) = $expr {
      return Ok(Some(node));
    }
  };
}

/// A helper macro for returning parse errors with context
///
/// This macro is used to cut down on boilerplate when returning errors in the various parsing functions.
/// This macro also calls `self.panic_mode` internally, and requires a mutable borrow of the '$tks' parameter.
macro_rules! bail {
	($parser:expr, $tks:expr, $($arg:tt)*) => {
		$parser.panic_mode(&mut $tks);
		return Err(parse_err!($parser, $tks, $($arg)*));
	};
}

/// A helper macro for constructing parse errors with context
macro_rules! parse_err {
	($parser:expr, $tks:expr, $($arg:tt)*) => {
		$crate::eval::parse::util::parse_err_full(
			&format!($($arg)*),
			&crate::eval::lex::TkVecUtils::get_span(&$tks).unwrap(),
			$parser.context.clone(),
		)
	};
}

/// A helper macro for constructing AST nodes with varying amounts of information
///
/// The first three parameters are always required, but the flags and redirs can be optionally left out if not needed. This is used to cut down on boilerplate when constructing nodes in the various parsing functions
/// example:
/// ```
/// node!(self, node_tks, NdRule::Conjunction { elements }, vec![], NdFlags::empty())
/// ```
macro_rules! node {
  ($parser:expr, $tks:expr, $class:expr, $redirs:expr, $flags:expr) => {
    $crate::eval::parse::node::Node {
      class: $class,
      flags: $flags,
      redirs: $redirs,
      context: $parser.context.clone(),
      tokens: $tks,
    }
  };
  ($parser:expr, $tks:expr, $class:expr, $redirs:expr) => {
    $crate::eval::parse::node::Node {
      class: $class,
      flags: $crate::eval::parse::node::NdFlags::empty(),
      redirs: $redirs,
      context: $parser.context.clone(),
      tokens: $tks,
    }
  };
  ($parser:expr, $tks:expr, $class:expr) => {
    $crate::eval::parse::node::Node {
      class: $class,
      flags: $crate::eval::parse::node::NdFlags::empty(),
      redirs: vec![],
      context: $parser.context.clone(),
      tokens: $tks,
    }
  };
}
