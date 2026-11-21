//! Small pieces every module leans on: one wall clock, host <-> container path
//! translation, and the macro that makes an enum serialize through its own
//! `as_str` / `parse`.

pub mod clock;
pub mod path_map;
#[macro_use]
pub mod serde_str;
