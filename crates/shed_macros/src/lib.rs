mod builtin;
mod shopts;

use proc_macro::TokenStream;

#[proc_macro_derive(ShOptGroup, attributes(validate, default, group_name))]
pub fn derive_shopt_group(input: TokenStream) -> TokenStream {
  shopts::derive_shopt_group(input)
}
