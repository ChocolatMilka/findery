//! What a vector is made of besides the text: the space it lives in
//! ([`recipe`]) and the prompt formats the model reads ([`prompt`]).
//!
//! INTENT: both decide which stored vectors a query may be compared with, so
//! they live side by side: a template that changes every vector enters the
//! recipe, and nothing else does.

pub mod prompt;
pub mod recipe;
