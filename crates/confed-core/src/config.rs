//! Argument resolution with strict precedence:
//!
//! 1. CLI flag
//! 2. environment (`CONFED_*`)
//! 3. stored config (from `init`, in `.state.db`/`.session.db`)
//! 4. interactive prompt — only when stdin is a TTY and neither
//!    `--non-interactive` nor `--json` was given
//!
//! In non-interactive mode step 4 becomes a fast, specific failure (exit 2)
//! rather than a hang. Every resolved value remembers where it came from so
//! `config --list` and `doctor` can show it.

use crate::error::{ConfedError, Result};
use std::collections::BTreeMap;

/// Where a resolved value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    Stored,
    Prompt,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Flag => "flag",
            Source::Env => "env",
            Source::Stored => "stored",
            Source::Prompt => "prompt",
            Source::Default => "default",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolved<T> {
    pub value: T,
    pub source: Source,
}

impl<T> Resolved<T> {
    pub fn new(value: T, source: Source) -> Self {
        Self { value, source }
    }
}

/// Describes one configurable value: how to name it in errors and where to look.
#[derive(Clone, Copy, Debug)]
pub struct ValueSpec {
    /// Human name used in messages, e.g. "space key".
    pub name: &'static str,
    pub flag: &'static str,
    pub env: &'static str,
    /// Key in the stored config.
    pub key: &'static str,
    /// Secrets are never echoed and are masked in `config --list`.
    pub secret: bool,
}

pub const BASE_URL: ValueSpec = ValueSpec {
    name: "Confluence base URL",
    flag: "--base-url",
    env: "CONFED_BASE_URL",
    key: "base_url",
    secret: false,
};
pub const TOKEN: ValueSpec = ValueSpec {
    name: "API token",
    flag: "--token",
    env: "CONFED_TOKEN",
    key: "token",
    secret: true,
};
pub const USERNAME: ValueSpec = ValueSpec {
    name: "username or email",
    flag: "--user",
    env: "CONFED_USERNAME",
    key: "username",
    secret: false,
};
pub const SPACE: ValueSpec = ValueSpec {
    name: "space key",
    flag: "--space",
    env: "CONFED_SPACE",
    key: "space",
    secret: false,
};
pub const FLAVOR: ValueSpec = ValueSpec {
    name: "Confluence flavor",
    flag: "--flavor",
    env: "CONFED_FLAVOR",
    key: "flavor",
    secret: false,
};
pub const CONCURRENCY: ValueSpec = ValueSpec {
    name: "request concurrency",
    flag: "--concurrency",
    env: "CONFED_CONCURRENCY",
    key: "concurrency",
    secret: false,
};
pub const EDITOR: ValueSpec = ValueSpec {
    name: "editor",
    flag: "--editor",
    env: "CONFED_EDITOR",
    key: "editor",
    secret: false,
};

/// Every value `confed config --set` accepts.
pub const SETTABLE: &[ValueSpec] = &[SPACE, CONCURRENCY, EDITOR, BASE_URL, FLAVOR];

/// Asks the user for a value. The CLI supplies a TTY implementation; tests and
/// non-interactive runs supply [`NoPrompt`].
pub trait Prompter: Send + Sync {
    /// Return `Ok(None)` when prompting is not possible.
    fn prompt(&self, spec: &ValueSpec) -> Result<Option<String>>;
}

/// Prompting disabled: `--non-interactive`, `--json`, or no TTY.
pub struct NoPrompt;

impl Prompter for NoPrompt {
    fn prompt(&self, _spec: &ValueSpec) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Scripted answers, for tests.
pub struct ScriptedPrompt(pub BTreeMap<&'static str, String>);

impl Prompter for ScriptedPrompt {
    fn prompt(&self, spec: &ValueSpec) -> Result<Option<String>> {
        Ok(self.0.get(spec.key).cloned())
    }
}

pub struct ConfigResolver {
    env: BTreeMap<String, String>,
    stored: BTreeMap<String, String>,
    prompter: Box<dyn Prompter>,
    interactive: bool,
}

impl ConfigResolver {
    /// Read the environment from the process.
    pub fn from_env(interactive: bool, prompter: Box<dyn Prompter>) -> Self {
        let env = std::env::vars()
            .filter(|(k, _)| k.starts_with("CONFED_"))
            .collect::<BTreeMap<_, _>>();
        Self { env, stored: BTreeMap::new(), prompter, interactive }
    }

    /// Explicit environment, for tests.
    pub fn with_env(
        env: BTreeMap<String, String>,
        interactive: bool,
        prompter: Box<dyn Prompter>,
    ) -> Self {
        Self { env, stored: BTreeMap::new(), prompter, interactive }
    }

    pub fn with_stored(mut self, stored: BTreeMap<String, String>) -> Self {
        self.stored = stored;
        self
    }

    pub fn set_stored(&mut self, key: &str, value: impl Into<String>) {
        self.stored.insert(key.to_string(), value.into());
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// Look through flag → env → stored, without prompting.
    pub fn lookup(&self, spec: &ValueSpec, flag: Option<&str>) -> Option<Resolved<String>> {
        if let Some(v) = flag.filter(|v| !v.is_empty()) {
            return Some(Resolved::new(v.to_string(), Source::Flag));
        }
        if let Some(v) = self.env.get(spec.env).filter(|v| !v.is_empty()) {
            return Some(Resolved::new(v.clone(), Source::Env));
        }
        if let Some(v) = self.stored.get(spec.key).filter(|v| !v.is_empty()) {
            return Some(Resolved::new(v.clone(), Source::Stored));
        }
        None
    }

    /// Full resolution. Prompts only when interactive; otherwise fails with a
    /// message naming the flag, the environment variable, and `confed init`.
    pub fn require(&self, spec: &ValueSpec, flag: Option<&str>) -> Result<Resolved<String>> {
        if let Some(found) = self.lookup(spec, flag) {
            return Ok(found);
        }
        if self.interactive {
            if let Some(answer) = self.prompter.prompt(spec)? {
                if !answer.is_empty() {
                    return Ok(Resolved::new(answer, Source::Prompt));
                }
            }
        }
        Err(ConfedError::missing_value(spec.name, spec.flag, spec.env))
    }

    /// Like [`require`], but falls back to a default instead of failing.
    pub fn or_default(
        &self,
        spec: &ValueSpec,
        flag: Option<&str>,
        default: impl Into<String>,
    ) -> Resolved<String> {
        self.lookup(spec, flag)
            .unwrap_or_else(|| Resolved::new(default.into(), Source::Default))
    }

    /// Every value confed knows about, with its source — powers `config --list`.
    pub fn describe(&self, specs: &[ValueSpec]) -> Vec<(ValueSpec, Option<Resolved<String>>)> {
        specs
            .iter()
            .map(|spec| {
                let resolved = self.lookup(spec, None).map(|mut r| {
                    if spec.secret {
                        r.value = "***".to_string();
                    }
                    r
                });
                (*spec, resolved)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn stored(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn flag_beats_env_beats_stored_beats_prompt() {
        let prompt: BTreeMap<&'static str, String> =
            [("space", "FROM_PROMPT".to_string())].into_iter().collect();
        let resolver = ConfigResolver::with_env(
            env(&[("CONFED_SPACE", "FROM_ENV")]),
            true,
            Box::new(ScriptedPrompt(prompt)),
        )
        .with_stored(stored(&[("space", "FROM_STORED")]));

        let r = resolver.require(&SPACE, Some("FROM_FLAG")).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("FROM_FLAG", Source::Flag));

        let r = resolver.require(&SPACE, None).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("FROM_ENV", Source::Env));

        let resolver = ConfigResolver::with_env(
            env(&[]),
            true,
            Box::new(ScriptedPrompt([("space", "FROM_PROMPT".to_string())].into_iter().collect())),
        )
        .with_stored(stored(&[("space", "FROM_STORED")]));
        let r = resolver.require(&SPACE, None).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("FROM_STORED", Source::Stored));

        let resolver = ConfigResolver::with_env(
            env(&[]),
            true,
            Box::new(ScriptedPrompt([("space", "FROM_PROMPT".to_string())].into_iter().collect())),
        );
        let r = resolver.require(&SPACE, None).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("FROM_PROMPT", Source::Prompt));
    }

    #[test]
    fn non_interactive_fails_fast_instead_of_prompting() {
        let resolver = ConfigResolver::with_env(env(&[]), false, Box::new(NoPrompt));
        let err = resolver.require(&SPACE, None).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::ExitCode::Usage);
        let hint = err.hint().unwrap();
        assert!(hint.contains("--space"), "{hint}");
        assert!(hint.contains("CONFED_SPACE"), "{hint}");
        assert!(hint.contains("confed init"), "{hint}");
    }

    #[test]
    fn interactive_but_unanswered_still_fails_rather_than_hanging() {
        let resolver = ConfigResolver::with_env(env(&[]), true, Box::new(NoPrompt));
        assert_eq!(
            resolver.require(&SPACE, None).unwrap_err().exit_code(),
            crate::error::ExitCode::Usage
        );
    }

    #[test]
    fn empty_values_are_treated_as_absent() {
        let resolver = ConfigResolver::with_env(env(&[("CONFED_SPACE", "")]), false, Box::new(NoPrompt))
            .with_stored(stored(&[("space", "DOCS")]));
        let r = resolver.require(&SPACE, Some("")).unwrap();
        assert_eq!((r.value.as_str(), r.source), ("DOCS", Source::Stored));
    }

    #[test]
    fn defaults_are_reported_as_such() {
        let resolver = ConfigResolver::with_env(env(&[]), false, Box::new(NoPrompt));
        let r = resolver.or_default(&CONCURRENCY, None, "4");
        assert_eq!((r.value.as_str(), r.source), ("4", Source::Default));
    }

    #[test]
    fn describe_masks_secrets_but_shows_sources() {
        let resolver = ConfigResolver::with_env(
            env(&[("CONFED_TOKEN", "super-secret"), ("CONFED_SPACE", "DOCS")]),
            false,
            Box::new(NoPrompt),
        );
        let described = resolver.describe(&[TOKEN, SPACE]);

        let token = described[0].1.as_ref().unwrap();
        assert_eq!(token.value, "***");
        assert_eq!(token.source, Source::Env);

        let space = described[1].1.as_ref().unwrap();
        assert_eq!(space.value, "DOCS");
    }

    #[test]
    fn unrelated_environment_variables_are_ignored() {
        let resolver =
            ConfigResolver::with_env(env(&[("SPACE", "WRONG")]), false, Box::new(NoPrompt));
        assert!(resolver.lookup(&SPACE, None).is_none());
    }
}
