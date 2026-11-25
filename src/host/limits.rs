//! What a `sandbox` run's container may use: the instance's ceiling, the one a repository's
//! `sandbox_limits` set in `repos.json`, and the `limits` a request asks for under it.

use serde_json::Value;

/// The most a request may ask for, and what it gets for a limit it leaves out, unless its
/// repository's `sandbox_limits` say otherwise.
pub const CEILING: Limits = Limits {
    memory_mb: 4096,
    cpus: 2.0,
    pids: 512,
    file_size_mb: 1024,
    disk_mb: 2048,
    timeout_secs: 1800,
};
/// The limits `limits` and `sandbox_limits` take.
const NAMES: [&str; 6] = [
    "memory_mb",
    "cpus",
    "pids",
    "file_size_mb",
    "disk_mb",
    "timeout_secs",
];
/// What the container may use; each is at most its repository's ceiling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub memory_mb: u64,
    pub cpus: f64,
    pub pids: u64,
    /// The largest file any process may write.
    pub file_size_mb: u64,
    /// The size of the scratch tmpfs.
    pub disk_mb: u64,
    pub timeout_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        CEILING
    }
}

/// A repository's `sandbox_limits`: the most its requests may ask for, each [`CEILING`]'s
/// when left out.
pub fn ceiling(value: &Value) -> Result<Limits, String> {
    read(value, "sandbox_limits", &CEILING, false)
}

/// A request's `limits`, each at most `ceiling`'s and `ceiling`'s when left out.
pub fn requested(value: &Value, ceiling: &Limits) -> Result<Limits, String> {
    read(value, "limits", ceiling, true)
}

/// The limits `value` gives over `base`'s, each above 0 and, when `capped`, at most `base`'s.
fn read(value: &Value, field: &str, base: &Limits, capped: bool) -> Result<Limits, String> {
    let given = match value {
        Value::Null => &serde_json::Map::new(),
        Value::Object(given) => given,
        _ => return Err(format!("`{field}` must be a JSON object")),
    };
    if let Some(name) = given.keys().find(|k| !NAMES.contains(&k.as_str())) {
        return Err(format!("`{field}` has no `{name}`; it takes {NAMES:?}"));
    }
    let at_most = |max: String| {
        if capped {
            format!(" and at most {max}")
        } else {
            String::new()
        }
    };
    let whole = |name: &str, base: u64| match given.get(name) {
        None => Ok(base),
        Some(v) => (v.as_u64())
            .filter(|n| *n >= 1 && (!capped || *n <= base))
            .ok_or_else(|| {
                let most = at_most(base.to_string());
                format!("`{field}.{name}` must be a whole number above 0{most}")
            }),
    };
    let cpus = match given.get("cpus") {
        None => base.cpus,
        Some(v) => (v.as_f64())
            .filter(|c| *c > 0.0 && (!capped || *c <= base.cpus))
            .ok_or_else(|| {
                format!(
                    "`{field}.cpus` must be above 0{}",
                    at_most(base.cpus.to_string())
                )
            })?,
    };
    Ok(Limits {
        memory_mb: whole("memory_mb", base.memory_mb)?,
        cpus,
        pids: whole("pids", base.pids)?,
        file_size_mb: whole("file_size_mb", base.file_size_mb)?,
        disk_mb: whole("disk_mb", base.disk_mb)?,
        timeout_secs: whole("timeout_secs", base.timeout_secs)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_repository_ceiling_replaces_the_limits_it_names() {
        let repo = ceiling(&json!({"memory_mb": 8192, "disk_mb": 12288, "timeout_secs": 3600}));
        let repo = repo.unwrap();
        assert_eq!(
            repo,
            Limits {
                memory_mb: 8192,
                disk_mb: 12288,
                timeout_secs: 3600,
                ..CEILING
            }
        );
        assert_eq!(ceiling(&Value::Null), Ok(CEILING), "no sandbox_limits");
        assert_eq!(ceiling(&json!({"cpus": 0.5})).unwrap().cpus, 0.5);
    }

    #[test]
    fn a_request_may_ask_up_to_its_repository_ceiling() {
        let repo = ceiling(&json!({"memory_mb": 8192, "disk_mb": 12288})).unwrap();
        assert_eq!(
            requested(&Value::Null, &repo),
            Ok(repo),
            "the ceiling when left out"
        );
        let most = json!({"memory_mb": 8192, "disk_mb": 12288});
        assert_eq!(requested(&most, &repo), Ok(repo));
        for over in [json!({"memory_mb": 8193}), json!({"disk_mb": 12289})] {
            let error = requested(&over, &repo).unwrap_err();
            assert!(error.contains("at most"), "{error}");
        }
        let default = json!({"memory_mb": CEILING.memory_mb + 1});
        assert!(requested(&default, &CEILING).is_err(), "no sandbox_limits");
    }

    #[test]
    fn refuses_repository_limits_that_are_not_numbers_above_zero() {
        for (bad, says) in [
            (
                json!({"memory_mb": 0}),
                "`sandbox_limits.memory_mb` must be a whole number above 0",
            ),
            (json!({"disk_mb": -1}), "`sandbox_limits.disk_mb`"),
            (json!({"pids": "512"}), "`sandbox_limits.pids`"),
            (
                json!({"timeout_secs": 1.5}),
                "`sandbox_limits.timeout_secs`",
            ),
            (json!({"cpus": 0}), "`sandbox_limits.cpus` must be above 0"),
            (json!({"cpus": "2"}), "`sandbox_limits.cpus`"),
            (json!({"swap_mb": 1}), "`sandbox_limits` has no `swap_mb`"),
            (json!([8192]), "`sandbox_limits` must be a JSON object"),
        ] {
            let error = ceiling(&bad).unwrap_err();
            assert!(error.starts_with(says), "{bad}: {error}");
        }
    }
}
