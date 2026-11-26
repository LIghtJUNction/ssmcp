use super::{Category, invalid, relative_path};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

/// Rules select registered files, never arbitrary filesystem matches.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupConfig {
    pub enabled: bool,
    pub max_age_seconds: u64,
    pub max_total_bytes: u64,
    pub rules: Vec<CleanupRule>,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_age_seconds: 24 * 60 * 60,
            max_total_bytes: 512 * 1024 * 1024,
            rules: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupRule {
    pub relative_prefix: String,
    pub category: Category,
    #[serde(default)]
    pub max_age_seconds: Option<u64>,
}

impl CleanupConfig {
    pub fn validate(&self) -> io::Result<()> {
        if self.rules.len() > 64 {
            return Err(invalid("cleanup supports at most 64 rules"));
        }
        for rule in &self.rules {
            let prefix = relative_path(Path::new(&rule.relative_prefix))?;
            if prefix != rule.relative_prefix
                || !Path::new(&prefix).starts_with(".ssmcp")
                || prefix == ".ssmcp"
                || super::protected_path(&prefix)
            {
                return Err(invalid(
                    "cleanup rule must name an unprotected path inside .ssmcp",
                ));
            }
            if rule.category != Category::Temporary {
                return Err(invalid(
                    "cleanup rules may only select registered temporary files",
                ));
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn released_job_eligible(&self, path: &Path, elapsed_seconds: u64) -> bool {
        // Explicitly disposable jobs release on forget/eviction or session close.
        // A custom rule can narrow that permission or require a retention period.
        self.enabled
            && (self.rules.is_empty()
                || path
                    .to_str()
                    .and_then(|p| self.age_for(p, Category::Temporary))
                    .is_some_and(|age| elapsed_seconds >= age))
    }

    pub(crate) fn age_for(&self, path: &str, category: Category) -> Option<u64> {
        if self.rules.is_empty() {
            return Some(self.max_age_seconds);
        }
        // Most specific component prefix wins; equal specificity uses the
        // first rule. A rule cannot broaden the hard category/path boundary.
        let mut selected = None;
        let mut depth = 0;
        for rule in &self.rules {
            if rule.category == category && Path::new(path).starts_with(&rule.relative_prefix) {
                let candidate_depth = Path::new(&rule.relative_prefix).components().count();
                if candidate_depth > depth {
                    depth = candidate_depth;
                    selected = Some(rule.max_age_seconds.unwrap_or(self.max_age_seconds));
                }
            }
        }
        selected
    }
}
