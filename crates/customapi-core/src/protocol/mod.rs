pub fn _rawcheck(s: &str) -> Box<serde_json::value::RawValue> {
    serde_json::value::RawValue::from_string(s.to_string()).unwrap()
}
