//! Makes an enum serialize through its own `as_str` / `parse` pair.
//!
//! INTENT: a derived `#[serde(rename_all = "snake_case")]` invents its own
//! spelling from the variant name — `OpenAiCompat` becomes `open_ai_compat` —
//! while `parse()` accepts `openai_compat` and a set of aliases. The two then
//! disagree, and the same value works through an environment variable but fails
//! in the configuration file. This macro removes the second spelling rule:
//! `as_str` and `parse` become the only ones, for every format at once.
//!
//! That is CONV-018 enforced rather than hoped for: one `as_str`, one `parse`,
//! and every wire format goes through them.

/// Implements `Serialize` and `Deserialize` in terms of `as_str` and `parse`.
///
/// Two forms, depending on what `parse` returns:
///
/// ```text
/// serde_via_parse!(MyEnum, option);   // fn parse(&str) -> Option<Self>
/// serde_via_parse!(MyEnum, result);   // fn parse(&str) -> crate::Result<Self>
/// ```
#[macro_export]
macro_rules! serde_via_parse {
    ($ty:ty, option) => {
        $crate::serde_via_parse!(@impl $ty, |raw: &str| <$ty>::parse(raw));
    };
    ($ty:ty, result) => {
        $crate::serde_via_parse!(@impl $ty, |raw: &str| <$ty>::parse(raw).ok());
    };
    (@impl $ty:ty, $to_opt:expr) => {
        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> ::core::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> ::core::result::Result<Self, D::Error> {
                let raw = <String as serde::Deserialize>::deserialize(d)?;
                let to_opt: fn(&str) -> Option<$ty> = $to_opt;
                to_opt(&raw).ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "unknown {} `{raw}`",
                        stringify!($ty)
                    ))
                })
            }
        }
    };
}
