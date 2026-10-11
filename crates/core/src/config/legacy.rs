//! Variables that were renamed, and the sentence that says so.
//!
//! A variable that silently stops being read leaves a deployment looking
//! configured while it is inert. Naming the replacement once, at boot, is the
//! fix — a warning, never an error, since the value is still in the file.
//!
//! Apart from [`super::env`]'s table because it answers another question: that
//! one says which variables are read now, this one which stopped being.

/// Names a renamed variable that is still set and no longer read.
///
/// Takes its lookup as a parameter so the whole rule is testable without
/// touching the process environment.
pub fn renamed_env_warning(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let set = |var: &str| get(var).is_some_and(|v| !v.is_empty());
    if set("FINDERY_CATALOG") && !set("FINDERY_BUCKET") {
        return Some(
            "FINDERY_CATALOG is set but no longer read — the setting is `FINDERY_BUCKET` \
             (file key `store.default_bucket`)"
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The renamed variable says so, and stays quiet once the new name is the
    /// one in use — a warning that fires on a correct deployment is noise nobody
    /// reads twice.
    #[test]
    fn a_renamed_variable_left_in_the_environment_names_its_replacement() {
        let legacy = renamed_env_warning(|k| (k == "FINDERY_CATALOG").then(|| "work".to_string()));
        assert!(legacy.expect("warning").contains("FINDERY_BUCKET"));

        let both = renamed_env_warning(|k| {
            matches!(k, "FINDERY_CATALOG" | "FINDERY_BUCKET").then(|| "work".to_string())
        });
        assert_eq!(both, None, "the new name wins, so there is nothing to say");

        let empty = renamed_env_warning(|k| (k == "FINDERY_CATALOG").then(String::new));
        assert_eq!(empty, None, "an empty variable overrides nothing");
    }

    /// Nothing set is the ordinary case, and it must cost nothing.
    #[test]
    fn an_untouched_environment_says_nothing() {
        assert_eq!(renamed_env_warning(|_| None), None);
    }
}
