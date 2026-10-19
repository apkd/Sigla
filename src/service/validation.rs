use serde_json::{Map, Value};

/// Use the advertised schema so required arguments and defaults have one source of truth.
pub(super) fn arguments(
    tool: &rmcp::model::Tool,
    args: Option<&Map<String, Value>>,
) -> Option<String> {
    if let Some(required) = tool.input_schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if args.is_none_or(|args| !args.contains_key(name)) {
                let hint = if name == "codebase" {
                    " Provide a local project path or repository URL."
                } else {
                    ""
                };
                return Some(format!("Missing required argument `{name}`.{hint}"));
            }
        }
    }
    let properties = tool.input_schema.get("properties")?.as_object()?;
    for (name, value) in args? {
        if properties
            .get(name)
            .and_then(|schema| schema.get("type"))
            .and_then(Value::as_str)
            == Some("string")
            && !value.is_string()
        {
            let received = match value {
                Value::Null => "null",
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::Array(_) => "an array",
                Value::Object(_) => "an object",
                Value::String(_) => unreachable!(),
            };
            return Some(format!(
                "Argument `{name}` must be a string; received {received}."
            ));
        }
    }
    None
}

pub(super) fn size(name: &str, value: &str, limit: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() <= limit,
        "Argument `{name}` exceeds the maximum size of {} bytes; received {} bytes. Shorten the {name}",
        bytes(limit),
        bytes(value.len()),
    );
    Ok(())
}

fn bytes(value: usize) -> String {
    let digits = value.to_string();
    let mut formatted = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(c);
    }
    formatted
}
