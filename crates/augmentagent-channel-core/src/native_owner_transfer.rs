//! Controller-owned runtime handoff; creation receipts remain immutable.
use anyhow::{Result,ensure};
use serde_json::Value;

pub fn canonical_bytes(value:&Value)->Result<Vec<u8>> {
    fn append(value:&Value,bytes:&mut Vec<u8>)->Result<()> {
        match value {
            Value::Null=>bytes.extend_from_slice(b"null"),
            Value::Bool(value)=>bytes.extend_from_slice(if *value {b"true"} else {b"false"}),
            Value::Number(value)=>{
                ensure!(value.is_i64() || value.is_u64(),"handoff JSON requires integer numbers");
                bytes.extend_from_slice(value.to_string().as_bytes());
            },
            Value::String(value)=>bytes.extend(serde_json::to_vec(value)?),
            Value::Array(values)=>{
                bytes.push(b'[');
                for (index,value) in values.iter().enumerate() {
                    if index>0 {bytes.push(b',');}
                    append(value,bytes)?;
                }
                bytes.push(b']');
            },
            Value::Object(values)=>{
                bytes.push(b'{');
                let mut entries:Vec<_>=values.iter().collect();
                entries.sort_by(|a,b|a.0.as_bytes().cmp(b.0.as_bytes()));
                for (index,(key,value)) in entries.into_iter().enumerate() {
                    if index>0 {bytes.push(b',');}
                    bytes.extend(serde_json::to_vec(key)?);bytes.push(b':');append(value,bytes)?;
                }
                bytes.push(b'}');
            },
        }
        Ok(())
    }
    let mut bytes=Vec::new();append(value,&mut bytes)?;Ok(bytes)
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
