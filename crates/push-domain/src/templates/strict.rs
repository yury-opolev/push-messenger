//! Strict JSON: a duplicate object key anywhere, or nesting deeper than `MAX_DEPTH`, is
//! refused, and objects keep their key order.

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

use super::TemplateError;

/// Deepest nesting of arrays and objects; the outermost container is level 1.
pub(crate) const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StrictValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<StrictValue>),
    Object(Vec<(String, StrictValue)>),
}

/// Parses one JSON document with nothing after it.
pub(crate) fn parse(bytes: &[u8]) -> Result<StrictValue, String> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value = Seed { depth: 0 }
        .deserialize(&mut de)
        .map_err(|e| e.to_string())?;
    de.end().map_err(|e| e.to_string())?;
    Ok(value)
}

/// An error at `path`, or without a location at the root.
pub(crate) fn at(path: &str, message: impl fmt::Display) -> TemplateError {
    if path.is_empty() {
        TemplateError(message.to_string())
    } else {
        TemplateError(format!("{path}: {message}"))
    }
}

/// `path.key`, or `key` at the root.
pub(crate) fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

struct Seed {
    depth: usize,
}

impl Seed {
    fn nested<E: de::Error>(&self) -> Result<Seed, E> {
        let depth = self.depth + 1;
        if depth > MAX_DEPTH {
            return Err(E::custom(format_args!(
                "JSON nests deeper than {MAX_DEPTH} levels"
            )));
        }
        Ok(Seed { depth })
    }
}

impl<'de> DeserializeSeed<'de> for Seed {
    type Value = StrictValue;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<StrictValue, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed {
    type Value = StrictValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<StrictValue, E> {
        Ok(StrictValue::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<StrictValue, E> {
        Ok(StrictValue::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<StrictValue, E> {
        Ok(StrictValue::Number(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<StrictValue, E> {
        Ok(StrictValue::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<StrictValue, E> {
        serde_json::Number::from_f64(v)
            .map(StrictValue::Number)
            .ok_or_else(|| E::custom("a number is not finite"))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<StrictValue, E> {
        Ok(StrictValue::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<StrictValue, E> {
        Ok(StrictValue::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<StrictValue, A::Error> {
        let inner = self.nested()?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(Seed { depth: inner.depth })? {
            items.push(item);
        }
        Ok(StrictValue::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<StrictValue, A::Error> {
        let inner = self.nested()?;
        let mut seen = BTreeSet::new();
        let mut fields = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format_args!("duplicate key {key:?}")));
            }
            let value = map.next_value_seed(Seed { depth: inner.depth })?;
            fields.push((key, value));
        }
        Ok(StrictValue::Object(fields))
    }
}

impl StrictValue {
    fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "a boolean",
            Self::Number(_) => "a number",
            Self::String(_) => "a string",
            Self::Array(_) => "an array",
            Self::Object(_) => "an object",
        }
    }

    fn mismatch(&self, path: &str, expected: &str) -> TemplateError {
        at(path, format!("must be {expected}, not {}", self.kind()))
    }

    pub(crate) fn object<'a>(&'a self, path: &str) -> Result<Object<'a>, TemplateError> {
        match self {
            Self::Object(fields) => Ok(Object {
                path: path.to_owned(),
                fields,
            }),
            _ => Err(self.mismatch(path, "an object")),
        }
    }

    pub(crate) fn array(&self, path: &str) -> Result<&[StrictValue], TemplateError> {
        match self {
            Self::Array(items) => Ok(items),
            _ => Err(self.mismatch(path, "an array")),
        }
    }

    pub(crate) fn string(&self, path: &str) -> Result<&str, TemplateError> {
        match self {
            Self::String(s) => Ok(s),
            _ => Err(self.mismatch(path, "a string")),
        }
    }

    pub(crate) fn boolean(&self, path: &str) -> Result<bool, TemplateError> {
        match self {
            Self::Bool(b) => Ok(*b),
            _ => Err(self.mismatch(path, "a boolean")),
        }
    }

    /// A JSON integer that fits an `i64`; `1.0` is not one.
    pub(crate) fn integer(&self, path: &str) -> Result<i64, TemplateError> {
        match self {
            Self::Number(n) => n
                .as_i64()
                .ok_or_else(|| at(path, format!("must be a 64-bit integer, not {n}"))),
            _ => Err(self.mismatch(path, "an integer")),
        }
    }
}

/// Compact JSON, objects in their stored order.
impl fmt::Display for StrictValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => write_string(f, s),
            Self::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Self::Object(fields) => {
                f.write_str("{")?;
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write_string(f, key)?;
                    write!(f, ":{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

fn write_string(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    f.write_str(&serde_json::to_string(s).map_err(|_| fmt::Error)?)
}

/// An object being read, with the path used in errors.
pub(crate) struct Object<'a> {
    path: String,
    fields: &'a [(String, StrictValue)],
}

impl<'a> Object<'a> {
    /// Refuses any field not in `known`.
    pub(crate) fn only(&self, known: &[&str]) -> Result<(), TemplateError> {
        match self
            .fields
            .iter()
            .find(|(key, _)| !known.contains(&key.as_str()))
        {
            Some((key, _)) => Err(at(&self.path, format!("unknown field {key:?}"))),
            None => Ok(()),
        }
    }

    pub(crate) fn fields(&self) -> &'a [(String, StrictValue)] {
        self.fields
    }

    pub(crate) fn path(&self, key: &str) -> String {
        join(&self.path, key)
    }

    pub(crate) fn get(&self, key: &str) -> Option<&'a StrictValue> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub(crate) fn field(&self, key: &str) -> Result<&'a StrictValue, TemplateError> {
        self.get(key)
            .ok_or_else(|| at(&self.path, format!("missing field {key:?}")))
    }

    pub(crate) fn string(&self, key: &str) -> Result<&'a str, TemplateError> {
        self.field(key)?.string(&self.path(key))
    }

    /// An optional boolean, `false` when absent.
    pub(crate) fn flag(&self, key: &str) -> Result<bool, TemplateError> {
        self.get(key)
            .map_or(Ok(false), |v| v.boolean(&self.path(key)))
    }

    pub(crate) fn integer(&self, key: &str) -> Result<i64, TemplateError> {
        self.field(key)?.integer(&self.path(key))
    }

    pub(crate) fn array(&self, key: &str) -> Result<&'a [StrictValue], TemplateError> {
        self.field(key)?.array(&self.path(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_key_order_and_writes_compact_json() {
        let text = r#"{ "b": [1, -2, 1.5, true, null], "a": {"x": "q\"\n"} }"#;
        let value = parse(text.as_bytes()).unwrap();
        assert_eq!(
            value.to_string(),
            r#"{"b":[1,-2,1.5,true,null],"a":{"x":"q\"\n"}}"#
        );
    }

    #[test]
    fn refuses_duplicate_keys_at_any_depth() {
        for text in [r#"{"a":1,"a":2}"#, r#"{"a":[{"b":1,"c":2,"b":3}]}"#] {
            let error = parse(text.as_bytes()).unwrap_err();
            assert!(error.contains("duplicate key"), "{error}");
        }
    }

    #[test]
    fn refuses_nesting_beyond_the_limit() {
        let nest = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(parse(nest(MAX_DEPTH).as_bytes()).is_ok());
        let error = parse(nest(MAX_DEPTH + 1).as_bytes()).unwrap_err();
        assert!(error.contains("deeper"), "{error}");
    }

    #[test]
    fn refuses_trailing_content() {
        assert!(parse(b"{} {}").is_err());
    }
}
