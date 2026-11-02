//! Serde helpers shared by the wire and profile types.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Writes a float the way Swift's and .NET's encoders do: integral values without
/// a fraction (`15`, not `15.0`), so a decoded file encodes back to the same text.
pub(crate) fn write_f64<S: Serializer>(value: f64, serializer: S) -> Result<S::Ok, S::Error> {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    if value.is_finite() && value.fract() == 0.0 && value.abs() < EXACT {
        #[allow(clippy::cast_possible_truncation)]
        let integral = value as i64;
        integral.serialize(serializer)
    } else {
        value.serialize(serializer)
    }
}

pub(crate) mod f64_plain {
    use super::*;
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub(crate) fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        write_f64(*value, serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        f64::deserialize(deserializer)
    }
}

pub(crate) mod f64_opt {
    use super::*;
    pub(crate) fn serialize<S: Serializer>(
        value: &Option<f64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => write_f64(*v, serializer),
            None => serializer.serialize_none(),
        }
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<f64>, D::Error> {
        Option::<f64>::deserialize(deserializer)
    }
}

pub(crate) mod f64_vec_opt {
    use super::*;
    use serde::ser::SerializeSeq;
    pub(crate) fn serialize<S: Serializer>(
        value: &Option<Vec<f64>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for v in values {
                    seq.serialize_element(&Float(*v))?;
                }
                seq.end()
            }
            None => serializer.serialize_none(),
        }
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<f64>>, D::Error> {
        Option::<Vec<f64>>::deserialize(deserializer)
    }
    struct Float(f64);
    impl Serialize for Float {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            write_f64(self.0, serializer)
        }
    }
}

/// Unknown JSON object members, kept as the text of a JSON object so they survive
/// a round trip through an older app (`None` when there are none). Used with
/// `#[serde(flatten, with = "extra")]` on an `Option<String>` field.
pub(crate) mod extra {
    use super::*;
    pub(crate) fn serialize<S: Serializer>(
        value: &Option<String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let map: Map<String, Value> = match value {
            Some(text) => serde_json::from_str(text).unwrap_or_default(),
            None => Map::new(),
        };
        map.serialize(serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        let map = Map::<String, Value>::deserialize(deserializer)?;
        Ok(if map.is_empty() {
            None
        } else {
            Some(Value::Object(map).to_string())
        })
    }
}

/// Modifier bits: any JSON integer is accepted at the type level and anything that
/// does not fit a byte fails with an "out of range" message; the per-context
/// maximum (31 stored, 15 on the wire) is checked by validation.
pub(crate) mod modifier_bits {
    use super::*;
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
        let value = i64::deserialize(deserializer)?;
        u8::try_from(value).map_err(|_| D::Error::custom(format!("modifiers {value} out of range")))
    }
}

pub(crate) mod modifier_bits_opt {
    use super::*;
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u8>, D::Error> {
        match Option::<i64>::deserialize(deserializer)? {
            None => Ok(None),
            Some(value) => u8::try_from(value)
                .map(Some)
                .map_err(|_| D::Error::custom(format!("modifiers {value} out of range"))),
        }
    }
}

pub(crate) mod hid_opt {
    use super::*;
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u16>, D::Error> {
        match Option::<i64>::deserialize(deserializer)? {
            None => Ok(None),
            Some(value) => u16::try_from(value)
                .map(Some)
                .map_err(|_| D::Error::custom(format!("hid {value} out of range 0...65535"))),
        }
    }
}

/// Deep JSON equality used by the merge rules ("the same profile again").
pub(crate) fn same_json<T: Serialize>(a: &T, b: &T) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Decoding where a JSON array never stands in for an object.
///
/// serde's derived `Deserialize` for a struct accepts a JSON array as well as an
/// object, filling the fields by position, so `{"source": ["display"]}` would
/// read as `{"source": {"kind": "display"}}`. No other Pad implementation does
/// that. Every decode of input JSON in this crate goes through [`from_str`] or
/// [`from_value`]: the text is read into a `serde_json::Value`, and the value is
/// handed to the type through [`Strict`], a deserializer over the value that
/// refuses anything but an object wherever the type asks for a struct, at every
/// depth (it wraps each nested value it hands on). The type's own `Deserialize`
/// is the shape: there is no second description of which members are objects.
///
/// One limit: internally tagged and untagged enums buffer their input inside
/// serde before decoding a variant, so the variant's own members are not seen
/// here. The crate's tagged enums (`OutgoingInput`, `PencilResponse`) are
/// outputs without a `Deserialize` implementation, which a test enforces.
///
/// A key repeated in one object keeps its last value (`serde_json::Value`'s
/// rule). Every decode in every app goes through this one parser, so no two
/// parsers can disagree about a repeated key.
pub(crate) mod strict {
    use serde::de::value::StringDeserializer;
    use serde::de::{
        self, DeserializeOwned, DeserializeSeed, IntoDeserializer, Unexpected, Visitor,
    };
    use serde_json::{Error, Map, Value};

    /// Reads JSON text as `T` (see the module documentation).
    pub(crate) fn from_str<T: DeserializeOwned>(text: &str) -> Result<T, Error> {
        from_value(serde_json::from_str::<Value>(text)?)
    }

    /// Reads a JSON value as `T` (see the module documentation).
    pub(crate) fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
        T::deserialize(Strict(value))
    }

    /// A `serde_json::Value` deserializer that accepts only objects for structs.
    pub(crate) struct Strict(pub(crate) Value);

    fn not_an_object(value: &Value) -> Error {
        let unexpected = match value {
            Value::Array(_) => Unexpected::Seq,
            Value::Null => Unexpected::Unit,
            Value::Bool(b) => Unexpected::Bool(*b),
            Value::Number(_) => Unexpected::Other("a number"),
            Value::String(s) => Unexpected::Str(s),
            Value::Object(_) => Unexpected::Map,
        };
        de::Error::invalid_type(unexpected, &"an object")
    }

    impl<'de> de::Deserializer<'de> for Strict {
        type Error = Error;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            match self.0 {
                Value::Array(items) => visitor.visit_seq(Seq(items.into_iter())),
                Value::Object(map) => visitor.visit_map(Members::new(map)),
                scalar => scalar.deserialize_any(visitor),
            }
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            _fields: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, Error> {
            match self.0 {
                Value::Object(map) => visitor.visit_map(Members::new(map)),
                other => Err(not_an_object(&other)),
            }
        }

        fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            match self.0 {
                Value::Null => visitor.visit_none(),
                other => visitor.visit_some(Strict(other)),
            }
        }

        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            visitor: V,
        ) -> Result<V::Value, Error> {
            visitor.visit_newtype_struct(self)
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            name: &'static str,
            variants: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, Error> {
            match self.0 {
                Value::Object(map) if map.len() == 1 => {
                    let Some((variant, content)) = map.into_iter().next() else {
                        return Err(de::Error::invalid_length(0, &"one variant"));
                    };
                    visitor.visit_enum(Variant { variant, content })
                }
                // Unit variants by name, and serde_json's own errors.
                other => other.deserialize_enum(name, variants, visitor),
            }
        }

        fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            visitor.visit_unit()
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf unit unit_struct seq tuple tuple_struct map identifier
        }
    }

    struct Seq(std::vec::IntoIter<Value>);

    impl<'de> de::SeqAccess<'de> for Seq {
        type Error = Error;

        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>, Error> {
            self.0
                .next()
                .map(|v| seed.deserialize(Strict(v)))
                .transpose()
        }

        fn size_hint(&self) -> Option<usize> {
            Some(self.0.len())
        }
    }

    struct Members {
        entries: serde_json::map::IntoIter,
        pending: Option<Value>,
    }

    impl Members {
        fn new(map: Map<String, Value>) -> Self {
            Self {
                entries: map.into_iter(),
                pending: None,
            }
        }
    }

    impl<'de> de::MapAccess<'de> for Members {
        type Error = Error;

        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, Error> {
            match self.entries.next() {
                Some((key, value)) => {
                    self.pending = Some(value);
                    let key: StringDeserializer<Error> = key.into_deserializer();
                    seed.deserialize(key).map(Some)
                }
                None => Ok(None),
            }
        }

        fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
            match self.pending.take() {
                Some(value) => seed.deserialize(Strict(value)),
                None => Err(de::Error::custom("value requested before its key")),
            }
        }

        fn size_hint(&self) -> Option<usize> {
            Some(self.entries.len())
        }
    }

    /// An externally tagged variant (`{"variant": content}`).
    struct Variant {
        variant: String,
        content: Value,
    }

    impl<'de> de::EnumAccess<'de> for Variant {
        type Error = Error;
        type Variant = Strict;

        fn variant_seed<V: DeserializeSeed<'de>>(
            self,
            seed: V,
        ) -> Result<(V::Value, Strict), Error> {
            let name: StringDeserializer<Error> = self.variant.into_deserializer();
            Ok((seed.deserialize(name)?, Strict(self.content)))
        }
    }

    impl<'de> de::VariantAccess<'de> for Strict {
        type Error = Error;

        fn unit_variant(self) -> Result<(), Error> {
            match self.0 {
                Value::Null => Ok(()),
                other => Err(de::Error::invalid_type(
                    Unexpected::Other(&other.to_string()),
                    &"a unit variant",
                )),
            }
        }

        fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
            seed.deserialize(self)
        }

        fn tuple_variant<V: Visitor<'de>>(
            self,
            _len: usize,
            visitor: V,
        ) -> Result<V::Value, Error> {
            de::Deserializer::deserialize_seq(self, visitor)
        }

        fn struct_variant<V: Visitor<'de>>(
            self,
            fields: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, Error> {
            de::Deserializer::deserialize_struct(self, "", fields, visitor)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde::Deserialize;
        use std::collections::HashMap;

        #[derive(Debug, PartialEq, Deserialize)]
        struct Inner {
            kind: String,
        }

        #[derive(Debug, PartialEq, Deserialize)]
        struct Outer {
            inner: Option<Inner>,
            #[serde(default)]
            list: Vec<Inner>,
            #[serde(default)]
            map: HashMap<String, Inner>,
            #[serde(default)]
            free: Option<Value>,
            #[serde(default)]
            numbers: Vec<f64>,
        }

        #[derive(Debug, PartialEq, Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Choice {
            Plain,
            Boxed(Inner),
            Shaped { inner: Inner },
        }

        /// Autoref specialization without dependencies: for a concrete `T`,
        /// `(&Probe::<T>(PhantomData)).deserializable()` resolves to the impl on
        /// `Probe<T>` (no autoref needed) when `T: DeserializeOwned`, and only
        /// otherwise falls back to the impl on `&Probe<T>`.
        struct Probe<T>(std::marker::PhantomData<T>);

        trait Deserializable {
            fn deserializable(&self) -> bool {
                true
            }
        }
        impl<T: DeserializeOwned> Deserializable for Probe<T> {}

        trait NotDeserializable {
            fn deserializable(&self) -> bool {
                false
            }
        }
        impl<T> NotDeserializable for &Probe<T> {}

        macro_rules! deserializable {
            ($ty:ty) => {
                (&Probe::<$ty>(std::marker::PhantomData)).deserializable()
            };
        }

        /// Internally tagged enums decode from serde's buffered copy of the
        /// input, past `Strict`; they must stay output-only.
        #[test]
        fn tagged_output_enums_cannot_be_decoded() {
            assert!(deserializable!(Inner), "the probe sees a derive");
            assert!(deserializable!(crate::profile::Profile));
            assert!(!deserializable!(crate::typing::OutgoingInput));
            assert!(!deserializable!(crate::typing::RoutedInput));
            assert!(!deserializable!(crate::actions::PencilResponse));
        }

        #[test]
        fn a_repeated_key_keeps_its_last_value() {
            let outer: Outer =
                from_str(r#"{"inner": {"kind": "a", "kind": "b"}, "inner": {"kind": "c"}}"#)
                    .unwrap();
            assert_eq!(outer.inner, Some(Inner { kind: "c".into() }));
            let outer: Outer = from_str(r#"{"inner": {"kind": "a", "kind": "b"}}"#).unwrap();
            assert_eq!(outer.inner, Some(Inner { kind: "b".into() }));
        }

        #[test]
        fn plain_serde_would_accept_arrays_for_structs() {
            let outer: Outer = serde_json::from_str(r#"{"inner": ["a"]}"#).unwrap();
            assert_eq!(outer.inner, Some(Inner { kind: "a".into() }));
            assert!(from_str::<Outer>(r#"{"inner": ["a"]}"#).is_err());
        }

        #[test]
        fn arrays_are_refused_wherever_an_object_belongs() {
            for bad in [
                r#"["a"]"#,
                r#"{"inner": ["a"]}"#,
                r#"{"list": [{"kind": "a"}, ["b"]]}"#,
                r#"{"map": {"x": ["a"]}}"#,
                r#"{"inner": 5}"#,
            ] {
                let error = from_str::<Outer>(bad).unwrap_err();
                assert!(error.to_string().contains("an object"), "{bad}: {error}");
            }
            assert!(from_str::<Choice>(r#"{"boxed": ["a"]}"#).is_err());
            assert!(from_str::<Choice>(r#"{"shaped": {"inner": ["a"]}}"#).is_err());
            assert!(from_str::<Choice>(r#"{"shaped": [{"kind": "a"}]}"#).is_err());
        }

        #[test]
        fn everything_else_decodes_as_before() {
            let text = r#"{"inner": {"kind": "a"}, "list": [{"kind": "b"}], "map": {"x": {"kind": "c"}}, "free": [1, {"y": [2]}], "numbers": [1, 2.5]}"#;
            let strict: Outer = from_str(text).unwrap();
            assert_eq!(strict, serde_json::from_str::<Outer>(text).unwrap());
            assert_eq!(strict.free, Some(serde_json::json!([1, {"y": [2]}])));
            assert_eq!(from_str::<Choice>(r#""plain""#).unwrap(), Choice::Plain);
            assert_eq!(
                from_str::<Choice>(r#"{"boxed": {"kind": "a"}}"#).unwrap(),
                Choice::Boxed(Inner { kind: "a".into() })
            );
            assert_eq!(
                from_str::<Choice>(r#"{"shaped": {"inner": {"kind": "a"}}}"#).unwrap(),
                Choice::Shaped {
                    inner: Inner { kind: "a".into() }
                }
            );
            assert!(!from_str::<Outer>("{").unwrap_err().is_data());
            assert!(from_str::<Outer>("{]").unwrap_err().is_syntax());
            assert!(
                from_str::<Outer>(r#"{"inner": ["a"]}"#)
                    .unwrap_err()
                    .is_data()
            );
        }
    }
}
