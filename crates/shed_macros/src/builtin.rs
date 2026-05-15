use syn::{Attribute, Expr, Ident, Token, punctuated::Punctuated};

fn extract_default(attrs: &[Attribute]) -> Option<Expr> {
  attrs
    .iter()
    .find(|a| a.path().is_ident("default"))
    .and_then(|a| a.parse_args::<Expr>().ok())
}

fn extract_kinds(attrs: &[Attribute]) -> Vec<String> {
  for a in attrs {
    if !a.path().is_ident("opt") {
      continue;
    }
    let idents = a
      .parse_args_with(Punctuated::<Ident, Token![,]>::parse_terminated)
      .unwrap();
    return idents.into_iter().map(|i| i.to_string()).collect();
  }
  vec![]
}
