//! Controller-owned runtime handoff; creation receipts remain immutable.
use anyhow::Result;
use serde_json::Value;

pub fn canonical_bytes(value:&Value)->Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handoff_wire_has_one_exact_encoding() {
        let value:Value=serde_json::from_str(r#"{"z":"é/\\\"\b\u0001","a":[{"b":2,"a":1}]}"#).unwrap();
        assert_eq!(canonical_bytes(&value).unwrap(),
            "{\"a\":[{\"a\":1,\"b\":2}],\"z\":\"é/\\\\\\\"\\b\\u0001\"}".as_bytes());
    }
    #[test]
    fn handoff_wire_refuses_floats_in_any_position() {
        for value in [serde_json::json!(1.5),serde_json::json!({"nested":[1,1.0]}),serde_json::json!(-0.0)] {
            assert!(canonical_bytes(&value).is_err(),"handoff accepted an ambiguous floating-point encoding: {value}");
        }
    }
}
