//! Private controller diagnostics, never admission or model-provided authority.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCapacityRefusal {
    category: String,
    execution_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    available_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    required_bytes: Option<u64>,
}
impl ToolCapacityRefusal {
    fn valid(&self) -> bool {
        matches!(
            self.category.as_str(),
            "build_scratch_space"
                | "build_storage_reserve"
                | "build_storage_busy"
                | "build_cache_full"
        )
    }
    pub fn capture(observation: &serde_json::Value) -> Option<Self> {
        let category = observation["tool_readiness"]["category"].as_str()?;
        if observation["exhausted"] != true
            || observation["phase"] != format!("native_tool_{category}")
        {
            return None;
        }
        let number = |key: &str| -> Option<Option<u64>> {
            match observation.get(key) {
                None => Some(None),
                Some(value) => Some(Some(value.as_u64()?)),
            }
        };
        let value = Self {
            category: category.into(),
            execution_started: observation["tool_readiness"]["execution_started"].as_bool()?,
            available_bytes: number("available_bytes")?,
            required_bytes: number("required_bytes")?,
        };
        value.valid().then_some(value)
    }
    pub fn render(&self) -> String {
        if !self.valid() {
            return "Build capacity observation unavailable".into();
        }
        let facts = match (self.available_bytes, self.required_bytes) {
            (Some(available), Some(required)) => {
                format!("; available {available} bytes, required {required} bytes")
            }
            _ => String::new(),
        };
        format!("Build capacity: {}{facts}", self.category)
    }
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(self.valid(), "invalid native tool capacity observation");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_only_bounded_controller_fields() {
        let mut value = serde_json::json!({"phase":"native_tool_build_storage_reserve","exhausted":true,
            "tool_readiness":{"category":"build_storage_reserve","execution_started":false},
            "available_bytes":25,"required_bytes":37,"private":"PRIVATE_COMMAND"});
        let observed = ToolCapacityRefusal::capture(&value).unwrap();
        assert!(!serde_json::to_string(&observed)
            .unwrap()
            .contains("PRIVATE_COMMAND"));
        assert!(observed
            .render()
            .contains("available 25 bytes, required 37 bytes"));
        value["available_bytes"] = (-1).into();
        assert!(ToolCapacityRefusal::capture(&value).is_none());
        value["available_bytes"] = 25.into();
        value["phase"] = "admission_failed".into();
        assert!(ToolCapacityRefusal::capture(&value).is_none());
        value["phase"] = "native_tool_unknown".into();
        value["tool_readiness"]["category"] = "unknown".into();
        assert!(ToolCapacityRefusal::capture(&value).is_none());
    }
}
