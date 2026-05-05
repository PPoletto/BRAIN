//! Per-path provider routing for internal LLM calls.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutingRule {
    pub path_prefix: String,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutingPolicy {
    pub default_provider: String,
    pub rules: Vec<RoutingRule>,
}

impl RoutingPolicy {
    pub fn provider_for(&self, path: &str) -> &str {
        // First match wins. Rules are applied in declaration order; a more
        // specific prefix should be listed earlier than a broader one.
        for rule in &self.rules {
            if path.starts_with(&rule.path_prefix) {
                return &rule.provider;
            }
        }
        &self.default_provider
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_policy() -> RoutingPolicy {
        RoutingPolicy {
            default_provider: "local".into(),
            rules: vec![
                RoutingRule {
                    path_prefix: "01_raw/email/personal/".into(),
                    provider: "local".into(),
                },
                RoutingRule {
                    path_prefix: "01_raw/email/work/".into(),
                    provider: "anthropic".into(),
                },
            ],
        }
    }

    #[test]
    fn provider_for_returns_default_when_no_rule_matches() {
        let p = sample_policy();
        assert_eq!(p.provider_for("02_wiki/topics/x.md"), "local");
    }

    #[test]
    fn provider_for_returns_rule_provider_when_prefix_matches() {
        let p = sample_policy();
        assert_eq!(
            p.provider_for("01_raw/email/work/2026/04/foo.eml"),
            "anthropic"
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let p = RoutingPolicy {
            default_provider: "openai".into(),
            rules: vec![
                RoutingRule {
                    path_prefix: "01_raw/email/personal/secret/".into(),
                    provider: "local".into(),
                },
                RoutingRule {
                    path_prefix: "01_raw/email/personal/".into(),
                    provider: "anthropic".into(),
                },
            ],
        };
        assert_eq!(
            p.provider_for("01_raw/email/personal/secret/x.eml"),
            "local"
        );
        assert_eq!(p.provider_for("01_raw/email/personal/x.eml"), "anthropic");
    }
}
