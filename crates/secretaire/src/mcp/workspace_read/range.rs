use serde::{Deserialize, Deserializer, de::Error};
use serde_json::{Number, Value, json};

#[derive(Clone, Copy)]
pub(in crate::mcp) struct IntegerRange {
    min: usize,
    max: usize,
}

impl IntegerRange {
    pub(in crate::mcp) const fn new(min: usize, max: usize) -> Self {
        Self { min, max }
    }

    pub(in crate::mcp) fn resolve(self, value: Option<i128>, default: usize) -> usize {
        self.clamp(value.unwrap_or(default as i128))
    }

    pub(in crate::mcp) fn clamp(self, value: i128) -> usize {
        value.clamp(self.min as i128, self.max as i128) as usize
    }

    pub(in crate::mcp) fn property(self, default: usize) -> Value {
        let mut schema = self.schema();
        schema["default"] = json!(default);
        schema
    }

    pub(super) fn schema(self) -> Value {
        json!({
            "type": "integer", "minimum": self.min, "maximum": self.max,
            "description": harness_text!(
                "Integers outside the range are clamped to the nearest bound.",
                "範囲外の整数は最寄りの境界値に丸める。"
            )
        })
    }
}

// Serde uses the field's default only when it is absent. A supplied value must
// be a JSON integer, including unsigned values above i64::MAX.
pub(in crate::mcp) fn optional_integer<'de, D>(deserializer: D) -> Result<Option<i128>, D::Error>
where
    D: Deserializer<'de>,
{
    let number =
        Number::deserialize(deserializer).map_err(|_| D::Error::custom("expected an integer"))?;
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
        .map(Some)
        .ok_or_else(|| D::Error::custom("expected an integer"))
}

#[cfg(test)]
pub(in crate::mcp) mod tests {
    use super::*;

    pub(in crate::mcp) fn boundary_cases(
        min: usize,
        max: usize,
        default: usize,
    ) -> [(Option<Value>, usize); 6] {
        [
            (None, default),
            (Some(json!(min as i64 - 1)), min),
            (Some(json!(min)), min),
            (Some(json!(max)), max),
            (Some(json!(max as u64 + 1)), max),
            (Some(json!(u64::MAX)), max),
        ]
    }

    pub(in crate::mcp) fn wrong_types() -> [Value; 4] {
        [json!("1"), json!(1.5), json!(true), Value::Null]
    }
}
