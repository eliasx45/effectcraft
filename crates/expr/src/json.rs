//! `__jparse(text)`: `JSON.parse` without boa's script parser.
//!
//! boa parses JSON by checking it with serde_json, then parsing, compiling and running it as a
//! script (`(text);`). Text style expressions parse their Source Text document on every
//! evaluation, which made that script parse about half of their time (and of a frame's
//! fingerprint). This builds the same value while serde_json reads the text: objects get their
//! keys in document order (the last of duplicate keys wins, as `CreateDataProperty` does),
//! numbers are doubles. Text it cannot read (nested deeper than serde_json's limit) returns
//! `undefined`, and the prelude falls back to `JSON.parse`.

use std::fmt;

use boa_engine::object::builtins::JsArray;
use boa_engine::{Context, JsObject, JsResult, JsString, JsValue};
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

/// The native function.
pub fn native_parse(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = args.first().cloned().unwrap_or_default().to_string(ctx)?;
    let Ok(text) = text.to_std_string() else { return Ok(JsValue::undefined()) };
    Ok(parse(&text, ctx).unwrap_or_default())
}

/// `text` as a JS value, or `None` when serde_json can't read it.
pub fn parse(text: &str, ctx: &mut Context) -> Option<JsValue> {
    let mut de = serde_json::Deserializer::from_str(text);
    let v = Build { ctx }.deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(v)
}

struct Build<'c> {
    ctx: &'c mut Context,
}

impl<'de> DeserializeSeed<'de> for Build<'_> {
    type Value = JsValue;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<JsValue, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Build<'_> {
    type Value = JsValue;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<JsValue, E> {
        Ok(JsValue::from(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<JsValue, E> {
        Ok(JsValue::from(v as f64))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<JsValue, E> {
        Ok(JsValue::from(v as f64))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<JsValue, E> {
        Ok(JsValue::from(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<JsValue, E> {
        Ok(JsValue::from(JsString::from(v)))
    }

    fn visit_unit<E: de::Error>(self) -> Result<JsValue, E> {
        Ok(JsValue::null())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<JsValue, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
        while let Some(v) = seq.next_element_seed(Build { ctx: &mut *self.ctx })? {
            items.push(v);
        }
        Ok(JsArray::from_iter(items, self.ctx).into())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JsValue, A::Error> {
        let obj = JsObject::with_object_proto(self.ctx.intrinsics());
        while let Some(k) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            let v = map.next_value_seed(Build { ctx: &mut *self.ctx })?;
            obj.create_data_property_or_throw(JsString::from(k.as_ref()), v, self.ctx).map_err(|e| de::Error::custom(e.to_string()))?;
        }
        Ok(obj.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `JSON.stringify` makes of `v`.
    fn back(v: &JsValue, ctx: &mut Context) -> String {
        let json = ctx.global_object().get(boa_engine::js_string!("JSON"), ctx).unwrap();
        let f = json.as_object().unwrap().get(boa_engine::js_string!("stringify"), ctx).unwrap();
        let s = f.as_callable().unwrap().call(&JsValue::undefined(), std::slice::from_ref(v), ctx).unwrap();
        s.to_string(ctx).unwrap().to_std_string_escaped()
    }

    fn js_parse(text: &str, ctx: &mut Context) -> JsValue {
        let json = ctx.global_object().get(boa_engine::js_string!("JSON"), ctx).unwrap();
        let f = json.as_object().unwrap().get(boa_engine::js_string!("parse"), ctx).unwrap();
        f.as_callable().unwrap().call(&JsValue::undefined(), &[JsValue::from(JsString::from(text))], ctx).unwrap()
    }

    #[test]
    fn parses_like_json_parse() {
        let mut ctx = Context::default();
        for text in [
            r#"{"doc":{"text":"Hi \"there\"\né😀","runs":[{"font":"Inter","size":72.5,"fill":[1,0.5,0,1]}],"b":true,"n":null},"ops":[]}"#,
            r#"{"z":1,"a":2,"10":3,"2":4,"z":5,"__proto__":6}"#,
            r#"[1.5e300,-0.0,-0,12345678901234567890,0.1,[],{}]"#,
            r#""just a string""#,
            r#"  42  "#,
        ] {
            let ours = parse(text, &mut ctx).unwrap();
            let theirs = js_parse(text, &mut ctx);
            assert_eq!(back(&ours, &mut ctx), back(&theirs, &mut ctx), "{text}");
        }
        // Negative zero stays negative, as with JSON.parse.
        let z = parse("-0.0", &mut ctx).unwrap();
        assert!(z.as_number().is_some_and(|x| x == 0.0 && x.is_sign_negative()));
    }

    #[test]
    fn unreadable_text_is_left_to_json_parse() {
        let mut ctx = Context::default();
        assert!(parse("{\"a\":", &mut ctx).is_none());
        assert!(parse("[1] x", &mut ctx).is_none());
        // Out of serde_json's range (JSON.parse gives Infinity).
        assert!(parse("1e400", &mut ctx).is_none_or(|v| v.as_number() == Some(f64::INFINITY)));
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(parse(&deep, &mut ctx).is_none());
        let v = native_parse(&JsValue::undefined(), &[JsValue::from(JsString::from("{"))], &mut ctx).unwrap();
        assert!(v.is_undefined());
    }
}
