//! Strict JSON helpers shared by catalogue parsing and publication binding.

/// `serde_json` intentionally keeps the final member of a duplicate-key
/// object. Catalogue documents are publication-bound input, so accepting that
/// ambiguity would let different readers resolve different assets.
pub(super) fn reject_duplicate_json_keys(body: &str) -> Result<(), String> {
    use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
    use std::fmt;

    struct Seed;
    impl<'de> DeserializeSeed<'de> for Seed {
        type Value = ();

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(RejectVisitor)
        }
    }

    struct RejectVisitor;
    impl<'de> Visitor<'de> for RejectVisitor {
        type Value = ();

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("JSON without duplicate object keys")
        }

        fn visit_bool<E>(self, _: bool) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }

        fn visit_i64<E>(self, _: i64) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }

        fn visit_u64<E>(self, _: u64) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }

        fn visit_f64<E>(self, _: f64) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }

        fn visit_str<E>(self, _: &str) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }

        fn visit_string<E>(self, _: String) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }

        fn visit_none<E>(self) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }

        fn visit_unit<E>(self) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<(), A::Error>
        where
            A: SeqAccess<'de>,
        {
            while sequence.next_element_seed(Seed)?.is_some() {}
            Ok(())
        }

        fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut keys = std::collections::BTreeSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key.clone()) {
                    return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
                }
                map.next_value_seed(Seed)?;
            }
            Ok(())
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(body);
    Seed.deserialize(&mut deserializer)
        .map_err(|error| error.to_string())?;
    deserializer.end().map_err(|error| error.to_string())
}
