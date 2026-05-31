#![warn(clippy::pedantic)]

mod builtin;
mod shopts;
mod styled;

use proc_macro::TokenStream;

#[proc_macro_derive(ShOptGroup, attributes(validate, default, group_name))]
pub fn derive_shopt_group(input: TokenStream) -> TokenStream {
  shopts::derive_shopt_group(input)
}
#[proc_macro]
pub fn styled_format(input: TokenStream) -> TokenStream {
  styled::styled_format(input)
}
