//! Controller-owned tool refusals are execution state, not another model task.
use crate::tool_audit::AuditLogger;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(crate) struct Refusal {
    category: &'static str,
    available: Option<u64>,
    required: Option<u64>,
    started: bool,
}
impl Refusal {
    pub(crate) fn category(
        category: &str,
        available: Option<u64>,
        required: Option<u64>,
    ) -> Option<Self> {
        let category = match category {
            "build_storage_busy" => "build_storage_busy",
            "build_storage_reserve" => "build_storage_reserve",
            "build_scratch_space" => "build_scratch_space",
            "build_cache_full" => "build_cache_full",
            "build_vm_unavailable" => "build_vm_unavailable",
            "build_scratch_unavailable" => "build_scratch_unavailable",
            "build_scratch_limits" => "build_scratch_limits",
            "mcp_start" => "mcp_start",
            "mcp_timeout" => "mcp_timeout",
            "mcp_tools" => "mcp_tools",
            _ => return None,
        };
        Some(Self {
            category,
            available,
            required,
            started: false,
        })
    }
    pub(crate) fn tool(name: &str, failed: bool, content: &str) -> Option<Self> {
        if !failed || name != "mcp__jarvis__Bash" {
            return None;
        }
        let (text, started) = if let Some(text) = content
            .strip_prefix("[runner=vm] ")
            .or_else(|| content.strip_prefix("[runner=host] "))
        {
            (text, true)
        } else {
            (content, false)
        };
        let category = text
            .strip_prefix("JARVIS_READINESS:")?
            .split_whitespace()
            .next()?;
        let mut refusal = Self::category(category, None, None)?;
        refusal.started = started;
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Evidence {
            category: String,
            available_bytes: Option<u64>,
            required_bytes: Option<u64>,
        }
        if let Some(raw) = text
            .lines()
            .find_map(|line| line.strip_prefix("JARVIS_READINESS_EVIDENCE:"))
        {
            if raw.len() <= 1024 {
                if let Ok(evidence) = serde_json::from_str::<Evidence>(raw) {
                    if evidence.category == category {
                        refusal.available = evidence.available_bytes;
                        refusal.required = evidence.required_bytes;
                    }
                }
            }
        }
        Some(refusal)
    }
    pub(crate) fn fail(
        &self,
        provider: &str,
        logger: Option<&Arc<AuditLogger>>,
    ) -> anyhow::Result<()> {
        if matches!(
            self.category,
            "build_storage_busy"
                | "build_storage_reserve"
                | "build_scratch_space"
                | "build_cache_full"
        ) {
            let mut evidence = serde_json::json!({"phase":format!("native_tool_{}",self.category),"exhausted":true,
                "tool_readiness":{"category":self.category,"execution_started":self.started}});
            if let Some(value) = self.available {
                evidence["available_bytes"] = value.into();
            }
            if let Some(value) = self.required {
                evidence["required_bytes"] = value.into();
            }
            crate::run_resources::admission_failed_observation(provider, logger, evidence)
        } else {
            Err(crate::reasoner::ReasonerError::Local {
                message:format!("{provider}: native build tool readiness unavailable ({}); inspect execution configuration",self.category)
            }.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_failed_controller_bash_results_can_yield_a_turn() {
        let text = "JARVIS_READINESS:build_scratch_space PRIVATE_PATH_AND_COMMAND";
        assert!(Refusal::tool("mcp__jarvis__Bash", true, text).is_some());
        for (name, failed, value) in [
            ("Bash", true, text),
            ("mcp__other__Bash", true, text),
            ("mcp__jarvis__Read", true, text),
            ("mcp__jarvis__Bash", false, text),
            (
                "mcp__jarvis__Bash",
                true,
                "quoted JARVIS_READINESS:build_scratch_space",
            ),
            (
                "mcp__jarvis__Bash",
                true,
                "{\"stderr\":\"JARVIS_READINESS:build_scratch_space\"}",
            ),
            (
                "mcp__jarvis__Bash",
                true,
                "JARVIS_READINESS:unknown private text",
            ),
        ] {
            assert!(Refusal::tool(name, failed, value).is_none());
        }
        let refusal = Refusal::tool("mcp__jarvis__Bash", true, text).unwrap();
        assert!(!format!("{refusal:?}").contains("PRIVATE"));
    }
    #[test]
    fn evidence_is_bounded_typed_and_cannot_replace_the_refusal_category() {
        let refusal=Refusal::tool("mcp__jarvis__Bash",true,
            "JARVIS_READINESS:build_scratch_space diagnostic\nJARVIS_READINESS_EVIDENCE:{\"category\":\"build_scratch_space\",\"available_bytes\":25,\"required_bytes\":32}").unwrap();
        assert_eq!((refusal.available, refusal.required), (Some(25), Some(32)));
        let mismatched=Refusal::tool("mcp__jarvis__Bash",true,
            "[runner=vm] JARVIS_READINESS:build_cache_full diagnostic\nJARVIS_READINESS_EVIDENCE:{\"category\":\"build_scratch_space\",\"available_bytes\":25,\"required_bytes\":32}").unwrap();
        assert!(mismatched.started);
        assert_eq!((mismatched.available, mismatched.required), (None, None));
    }
}
