//! A tool's arguments, read strictly: a key the schema does not declare, a
//! value of the wrong type, or a name outside its list is refused — and the
//! refusal names what is accepted.
//!
//! INTENT: a silently dropped `langs: ["cobol"]` is a search that ignores what
//! the agent asked for, and the agent has no way to see that it did. An error
//! listing the valid values is one retry away from the right call.

use serde_json::{Map, Value};

use findery_core::{Error, Result};

/// The arguments of one `tools/call`.
pub struct Args<'a> {
    tool: &'a str,
    map: Option<&'a Map<String, Value>>,
}

impl<'a> Args<'a> {
    /// Accepts an object whose keys are all in `declared`, or nothing at all.
    pub fn new(tool: &'a str, raw: &'a Value, declared: &[String]) -> Result<Self> {
        let map = match raw {
            Value::Null => None,
            Value::Object(map) => Some(map),
            other => {
                return Err(Error::Invalid(format!(
                    "`{tool}` takes an object of arguments, got {}",
                    kind_of(other)
                )));
            }
        };
        let unknown = map
            .into_iter()
            .flat_map(|m| m.keys())
            .find(|key| !declared.contains(key));
        if let Some(key) = unknown {
            return Err(Error::Invalid(format!(
                "`{tool}` has no argument `{key}`; it takes: {}",
                declared.join(", ")
            )));
        }
        Ok(Self { tool, map })
    }

    /// The tool these arguments were sent to.
    pub fn tool(&self) -> &str {
        self.tool
    }

    /// A string, trimmed; absent, `null` and blank all read as `None`.
    pub fn text(&self, key: &str) -> Result<Option<String>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
            Some(other) => Err(self.wrong_type(key, "a string", other)),
        }
    }

    /// A string that must be there.
    pub fn required(&self, key: &str) -> Result<String> {
        self.text(key)?
            .ok_or_else(|| Error::Invalid(format!("`{}` needs `{key}`", self.tool)))
    }

    pub fn flag(&self, key: &str) -> Result<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(other) => Err(self.wrong_type(key, "true or false", other)),
        }
    }

    /// A whole number, at least zero.
    pub fn count(&self, key: &str) -> Result<Option<u64>> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| self.wrong_type(key, "a whole number ≥ 0", value)),
        }
    }

    /// A list of strings; a lone string is read as a list of one.
    pub fn texts(&self, key: &str) -> Result<Vec<String>> {
        let items = match self.get(key) {
            None => return Ok(Vec::new()),
            Some(Value::String(one)) => return Ok(vec![one.trim().to_string()]),
            Some(Value::Array(items)) => items,
            Some(other) => return Err(self.wrong_type(key, "a list of strings", other)),
        };
        items
            .iter()
            .map(|item| match item {
                Value::String(s) => Ok(s.trim().to_string()),
                other => Err(self.wrong_type(key, "a list of strings", other)),
            })
            .collect()
    }

    /// One name of a closed list.
    pub fn choice<T>(
        &self,
        key: &str,
        parse: impl Fn(&str) -> Option<T>,
        valid: &[&str],
    ) -> Result<Option<T>> {
        match self.text(key)? {
            None => Ok(None),
            Some(raw) => parse(&raw)
                .map(Some)
                .ok_or_else(|| unknown_value(key, &raw, valid)),
        }
    }

    /// Several names of a closed list.
    pub fn choices<T>(
        &self,
        key: &str,
        parse: impl Fn(&str) -> Option<T>,
        valid: &[&str],
    ) -> Result<Vec<T>> {
        self.texts(key)?
            .iter()
            .map(|raw| parse(raw).ok_or_else(|| unknown_value(key, raw, valid)))
            .collect()
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.map?.get(key).filter(|v| !v.is_null())
    }

    fn wrong_type(&self, key: &str, wanted: &str, got: &Value) -> Error {
        Error::Invalid(format!(
            "`{}`: `{key}` must be {wanted}, got {}",
            self.tool,
            kind_of(got)
        ))
    }
}

/// The refusal of a name outside its list, listing the list.
pub fn unknown_value(key: &str, raw: &str, valid: &[&str]) -> Error {
    Error::Invalid(format!(
        "unknown `{key}` value `{raw}`; valid: {}",
        valid.join(", ")
    ))
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn declared() -> Vec<String> {
        ["query", "langs", "k", "raw"].map(String::from).to_vec()
    }

    #[test]
    fn declared_arguments_are_read_with_their_types() {
        let raw = json!({ "query": "  retry ", "langs": ["rust", "go"], "k": 3, "raw": true });
        let args = Args::new("search", &raw, &declared()).expect("valid");
        assert_eq!(args.required("query").expect("query"), "retry");
        assert_eq!(args.texts("langs").expect("langs"), ["rust", "go"]);
        assert_eq!(args.count("k").expect("k"), Some(3));
        assert_eq!(args.flag("raw").expect("raw"), Some(true));
    }

    #[test]
    fn an_undeclared_key_is_refused_and_the_declared_ones_listed() {
        let raw = json!({ "querry": "x" });
        let err = Args::new("search", &raw, &declared())
            .err()
            .expect("refused");
        let text = err.to_string();
        assert!(
            text.contains("`querry`") && text.contains("query, langs, k, raw"),
            "{text}"
        );
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused_rather_than_ignored() {
        let raw = json!({ "k": "eight", "langs": [1] });
        let args = Args::new("search", &raw, &declared()).expect("keys are fine");
        assert!(
            args.count("k")
                .expect_err("string")
                .to_string()
                .contains("whole number")
        );
        assert!(args.texts("langs").is_err());
        assert!(Args::new("search", &json!([1]), &declared()).is_err());
    }

    #[test]
    fn a_name_outside_its_list_is_refused_with_the_valid_ones() {
        let raw = json!({ "langs": ["rust", "cobol"] });
        let args = Args::new("search", &raw, &declared()).expect("valid");
        let parse = |s: &str| ["rust", "go"].contains(&s).then(|| s.to_string());
        let err = args
            .choices("langs", parse, &["rust", "go"])
            .expect_err("cobol");
        assert_eq!(
            err.to_string(),
            "invalid: unknown `langs` value `cobol`; valid: rust, go"
        );
    }

    #[test]
    fn nothing_blank_and_null_all_read_as_absent() {
        let raw = json!({ "query": "   ", "k": null });
        let args = Args::new("search", &raw, &declared()).expect("valid");
        assert_eq!(args.text("query").expect("blank"), None);
        assert_eq!(args.count("k").expect("null"), None);
        assert!(args.required("query").is_err());
        let none = Args::new("search", &Value::Null, &declared()).expect("no arguments");
        assert!(none.texts("langs").expect("absent").is_empty());
    }
}
