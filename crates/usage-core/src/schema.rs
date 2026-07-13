use schemars::{schema_for, JsonSchema};
use serde_json::{json, Value};

use crate::{RequestEnvelope, ResponseEnvelope, API_VERSION};

/// Generates the JSON Schema for protocol v3 request envelopes.
pub fn request_schema_v3() -> Value {
    versioned_schema::<RequestEnvelope>()
}

/// Generates the JSON Schema for protocol v3 response envelopes.
pub fn response_schema_v3() -> Value {
    versioned_schema::<ResponseEnvelope>()
}

fn versioned_schema<T: JsonSchema>() -> Value {
    let mut schema =
        serde_json::to_value(schema_for!(T)).expect("schema serialization must succeed");
    pin_api_version(&mut schema);
    schema
}

fn pin_api_version(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Object(properties)) = object.get_mut("properties") {
                if properties.contains_key("api_version") {
                    properties.insert(
                        "api_version".to_string(),
                        json!({"type": "integer", "const": API_VERSION}),
                    );
                }
            }
            for child in object.values_mut() {
                pin_api_version(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                pin_api_version(child);
            }
        }
        _ => {}
    }
}
