pub fn error_reason(_body: &[u8]) -> String {
    "tool".to_string()
}

pub fn local(body: &[u8]) -> String {
    error_reason(body)
}
