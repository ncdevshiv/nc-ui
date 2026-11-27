//! Shared `serde_json` validation for provider props and command payloads.
//!
//! The helpers exist because serde's type errors are positional rather than contractual:
//! "expected a string at column 19" is true, but it does not tell a page author which
//! field in a 64 KiB document failed. Shape validation therefore names the field first,
//! and only afterwards is the already-checked document decoded into the typed props
//! struct the widget implementation uses.

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::KitProviderError;

pub(crate) type Object = Map<String, Value>;

pub(crate) fn decode<T>(
    provider: &'static str,
    value: &Value,
    allowed_fields: &[&str],
    validate_shape: impl FnOnce(&Object) -> Result<(), KitProviderError>,
) -> Result<T, KitProviderError>
where
    T: DeserializeOwned,
{
    let object = match value {
        Value::Null => Object::new(),
        Value::Object(object) => object.clone(),
        other => {
            return Err(KitProviderError::PropsNotObject {
                provider,
                actual: json_kind(other),
            });
        }
    };
    reject_unknown_fields(provider, &object, allowed_fields)?;
    validate_shape(&object)?;
    serde_json::from_value(Value::Object(object)).map_err(|error| {
        invalid_props(
            provider,
            "props",
            format!("the JSON does not match the typed schema: {error}"),
        )
    })
}

pub(crate) fn invalid_props(
    provider: &'static str,
    field: impl Into<String>,
    reason: impl Into<String>,
) -> KitProviderError {
    KitProviderError::InvalidProps {
        provider,
        field: field.into(),
        reason: reason.into(),
    }
}

pub(crate) fn invalid_payload(
    provider: &'static str,
    action: &str,
    field: impl Into<String>,
    reason: impl Into<String>,
) -> KitProviderError {
    KitProviderError::InvalidActionPayload {
        provider,
        action: action.to_owned(),
        field: field.into(),
        reason: reason.into(),
    }
}

pub(crate) fn action_unavailable(
    provider: &'static str,
    action: &str,
    reason: impl Into<String>,
) -> KitProviderError {
    KitProviderError::ActionUnavailable {
        provider,
        action: action.to_owned(),
        reason: reason.into(),
    }
}

pub(crate) fn unknown_action(provider: &'static str, action: &str) -> KitProviderError {
    KitProviderError::UnknownAction {
        provider,
        action: action.to_owned(),
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

pub(crate) fn reject_unknown_fields(
    provider: &'static str,
    object: &Object,
    allowed_fields: &[&str],
) -> Result<(), KitProviderError> {
    for key in object.keys() {
        if !allowed_fields.contains(&key.as_str()) {
            return Err(invalid_props(
                provider,
                key.clone(),
                format!("is unknown; expected one of {}", allowed_fields.join(", ")),
            ));
        }
    }
    Ok(())
}

pub(crate) fn require_field(
    provider: &'static str,
    object: &Object,
    field: &str,
) -> Result<(), KitProviderError> {
    match object.get(field) {
        Some(Value::Null) | None => Err(invalid_props(provider, field, "is required")),
        Some(_) => Ok(()),
    }
}

pub(crate) fn check_string(
    provider: &'static str,
    object: &Object,
    field: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), KitProviderError> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    let Value::String(text) = value else {
        return Err(invalid_props(
            provider,
            field,
            format!("must be a string, not {}", json_kind(value)),
        ));
    };
    validate_string_text(provider, field, text, max_bytes, allow_empty)
}

pub(crate) fn validate_string_text(
    provider: &'static str,
    field: &str,
    text: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), KitProviderError> {
    if !allow_empty && text.is_empty() {
        return Err(invalid_props(provider, field, "must not be empty"));
    }
    if text.len() > max_bytes {
        return Err(invalid_props(
            provider,
            field,
            format!("is {} bytes, over the {max_bytes} byte bound", text.len()),
        ));
    }
    Ok(())
}

pub(crate) fn check_nullable_string(
    provider: &'static str,
    object: &Object,
    field: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), KitProviderError> {
    if matches!(object.get(field), Some(Value::Null)) {
        return Ok(());
    }
    check_string(provider, object, field, max_bytes, allow_empty)
}

pub(crate) fn check_boolean(
    provider: &'static str,
    object: &Object,
    field: &str,
) -> Result<(), KitProviderError> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    if value.is_boolean() {
        return Ok(());
    }
    Err(invalid_props(
        provider,
        field,
        format!("must be a boolean, not {}", json_kind(value)),
    ))
}

pub(crate) fn check_integer(
    provider: &'static str,
    object: &Object,
    field: &str,
    minimum: usize,
    maximum: usize,
) -> Result<(), KitProviderError> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    let Some(number) = value.as_u64() else {
        return Err(invalid_props(
            provider,
            field,
            format!("must be a non-negative integer, not {}", json_kind(value)),
        ));
    };
    let number = usize::try_from(number).unwrap_or(usize::MAX);
    if number < minimum || number > maximum {
        return Err(invalid_props(
            provider,
            field,
            format!("must be in the range {minimum}..={maximum}, not {number}"),
        ));
    }
    Ok(())
}

pub(crate) fn check_number(
    provider: &'static str,
    object: &Object,
    field: &str,
    minimum: f64,
    maximum: f64,
) -> Result<(), KitProviderError> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    let Some(number) = value.as_f64() else {
        return Err(invalid_props(
            provider,
            field,
            format!("must be a number, not {}", json_kind(value)),
        ));
    };
    if number < minimum || number > maximum {
        return Err(invalid_props(
            provider,
            field,
            format!("must be in the range {minimum}..={maximum}, not {number}"),
        ));
    }
    Ok(())
}

pub(crate) fn check_enum(
    provider: &'static str,
    object: &Object,
    field: &str,
    choices: &[&str],
) -> Result<(), KitProviderError> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    let Value::String(choice) = value else {
        return Err(invalid_props(
            provider,
            field,
            format!(
                "must be a string naming one of {}, not {}",
                choices.join(", "),
                json_kind(value)
            ),
        ));
    };
    if choices.contains(&choice.as_str()) {
        return Ok(());
    }
    Err(invalid_props(
        provider,
        field,
        format!("must be one of {}, not `{choice}`", choices.join(", ")),
    ))
}

pub(crate) fn check_array(
    provider: &'static str,
    object: &Object,
    field: &str,
    minimum: usize,
    maximum: usize,
) -> Result<(), KitProviderError> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    let Value::Array(items) = value else {
        return Err(invalid_props(
            provider,
            field,
            format!("must be an array, not {}", json_kind(value)),
        ));
    };
    if items.len() < minimum || items.len() > maximum {
        return Err(invalid_props(
            provider,
            field,
            format!(
                "must contain {minimum}..={maximum} items, not {}",
                items.len()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn check_string_array(
    provider: &'static str,
    object: &Object,
    field: &str,
    minimum: usize,
    maximum: usize,
    max_bytes: usize,
) -> Result<(), KitProviderError> {
    check_array(provider, object, field, minimum, maximum)?;
    let Some(Value::Array(items)) = object.get(field) else {
        return Ok(());
    };
    for (index, item) in items.iter().enumerate() {
        let item_field = format!("{field}[{index}]");
        let Value::String(text) = item else {
            return Err(invalid_props(
                provider,
                item_field,
                format!("must be a string, not {}", json_kind(item)),
            ));
        };
        validate_string_text(provider, &item_field, text, max_bytes, false)?;
    }
    Ok(())
}

pub(crate) fn check_object(
    provider: &'static str,
    object: &Object,
    field: &str,
) -> Result<(), KitProviderError> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    if value.is_object() {
        return Ok(());
    }
    Err(invalid_props(
        provider,
        field,
        format!("must be an object, not {}", json_kind(value)),
    ))
}

pub(crate) fn ensure_empty_payload(
    provider: &'static str,
    action: &str,
    payload: &Value,
) -> Result<(), KitProviderError> {
    match payload {
        Value::Null => Ok(()),
        Value::Object(object) if object.is_empty() => Ok(()),
        other => Err(invalid_payload(
            provider,
            action,
            "payload",
            format!("must be null or an empty object, not {}", json_kind(other)),
        )),
    }
}

pub(crate) fn payload_object<'a>(
    provider: &'static str,
    action: &str,
    payload: &'a Value,
    allowed_fields: &[&str],
) -> Result<&'a Object, KitProviderError> {
    let Value::Object(object) = payload else {
        return Err(invalid_payload(
            provider,
            action,
            "payload",
            format!("must be an object, not {}", json_kind(payload)),
        ));
    };
    for key in object.keys() {
        if !allowed_fields.contains(&key.as_str()) {
            return Err(invalid_payload(
                provider,
                action,
                key.clone(),
                format!("is unknown; expected one of {}", allowed_fields.join(", ")),
            ));
        }
    }
    Ok(object)
}

fn payload_field<'a>(
    provider: &'static str,
    action: &str,
    object: &'a Object,
    field: &str,
) -> Result<&'a Value, KitProviderError> {
    match object.get(field) {
        Some(Value::Null) | None => Err(invalid_payload(provider, action, field, "is required")),
        Some(value) => Ok(value),
    }
}

pub(crate) fn payload_boolean(
    provider: &'static str,
    action: &str,
    object: &Object,
    field: &str,
) -> Result<bool, KitProviderError> {
    let value = payload_field(provider, action, object, field)?;
    value.as_bool().ok_or_else(|| {
        invalid_payload(
            provider,
            action,
            field,
            format!("must be a boolean, not {}", json_kind(value)),
        )
    })
}

pub(crate) fn payload_string<'a>(
    provider: &'static str,
    action: &str,
    object: &'a Object,
    field: &str,
) -> Result<&'a str, KitProviderError> {
    let value = payload_field(provider, action, object, field)?;
    value.as_str().ok_or_else(|| {
        invalid_payload(
            provider,
            action,
            field,
            format!("must be a string, not {}", json_kind(value)),
        )
    })
}

pub(crate) fn payload_usize(
    provider: &'static str,
    action: &str,
    object: &Object,
    field: &str,
) -> Result<usize, KitProviderError> {
    let value = payload_field(provider, action, object, field)?;
    let Some(number) = value.as_u64() else {
        return Err(invalid_payload(
            provider,
            action,
            field,
            format!("must be a non-negative integer, not {}", json_kind(value)),
        ));
    };
    usize::try_from(number).map_err(|_| {
        invalid_payload(
            provider,
            action,
            field,
            format!("is too large for this platform: {number}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize)]
    struct Example {
        #[serde(default)]
        label: String,
    }

    fn shape(object: &Object) -> Result<(), KitProviderError> {
        check_string("provider", object, "label", 16, false)
    }

    #[test]
    fn typed_props_decode_after_shape_validation() {
        let props: Example = decode("provider", &json!({"label": "Save"}), &["label"], shape)
            .expect("valid props decode");
        assert_eq!(props.label, "Save");
    }

    #[test]
    fn a_wrong_field_type_names_the_field() {
        let error = decode::<Example>("provider", &json!({"label": 3}), &["label"], shape)
            .expect_err("the wrong type is refused");
        assert_eq!(error.code(), "kit_invalid_props");
        assert!(error.to_string().contains("label"));
    }

    #[test]
    fn an_unknown_field_is_named_in_the_refusal() {
        let error = decode::<Example>("provider", &json!({"labl": "Save"}), &["label"], shape)
            .expect_err("unknown fields are refused");
        assert!(error.to_string().contains("labl"));
    }

    #[test]
    fn a_field_level_null_is_not_a_synonym_for_omission() {
        let error = decode::<Example>("provider", &json!({"label": null}), &["label"], shape)
            .expect_err("only the top-level null is the empty-object shorthand");
        assert!(error.to_string().contains("label"));
        assert!(error.to_string().contains("null"));
    }

    #[test]
    fn action_payload_helpers_name_the_payload_field() {
        let payload = json!({"checked": "yes"});
        let object = payload_object("provider", "set_checked", &payload, &["checked"])
            .expect("the object shape is accepted");
        let error = payload_boolean("provider", "set_checked", object, "checked")
            .expect_err("the wrong type is refused");
        assert!(error.to_string().contains("checked"));
    }

    #[test]
    fn an_unknown_action_payload_field_is_refused_by_name() {
        let payload = json!({"checked": true, "cheked": false});
        let error = payload_object("provider", "set_checked", &payload, &["checked"])
            .expect_err("a mistyped payload key is not ignored");
        assert_eq!(error.code(), "kit_invalid_action_payload");
        assert!(error.to_string().contains("cheked"));
    }

    #[test]
    fn a_parameterless_command_rejects_a_nonempty_payload() {
        let error = ensure_empty_payload("provider", "click", &json!({"force": true}))
            .expect_err("click has no parameters");
        assert_eq!(error.code(), "kit_invalid_action_payload");
        assert!(error.to_string().contains("payload"));
    }
}
