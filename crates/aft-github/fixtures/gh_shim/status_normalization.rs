// Only explicitly host-dependent top-level values are normalized. All other
// bytes, including whitespace and field order, remain exactly as emitted.
pub fn normalize_v19_status(raw: &str) -> Result<String, String> {
    let _: serde_json::Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let mut output = raw.to_string();
    for (key, replacement) in [
        ("shim_version", "\"<VERSION_PROVENANCE>\""),
        ("executing_image", "\"<EXECUTING_IMAGE>\""),
        ("manifests_dir", "\"<STATE_DIR>/manifests\""),
        ("real_gh_resolution", "null"),
        ("real_gh_resolution_error", "\"<HOST_PATH_RESOLUTION>\""),
    ] {
        let marker = format!("\"{key}\":");
        if output.match_indices(&marker).count() != 1 {
            return Err(format!("expected one status field {key}"));
        }
        let mut start = output.find(&marker).unwrap() + marker.len();
        while output.as_bytes()[start].is_ascii_whitespace() {
            start += 1;
        }
        let bytes = output.as_bytes();
        let mut end = start;
        let mut quoted = false;
        let mut escaped = false;
        let mut depth = 0_usize;
        loop {
            let byte = *bytes.get(end).ok_or("unterminated status value")?;
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                    if depth == 0 {
                        end += 1;
                        break;
                    }
                }
            } else {
                match byte {
                    b'"' => quoted = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' if depth > 0 => {
                        depth -= 1;
                        if depth == 0 {
                            end += 1;
                            break;
                        }
                    }
                    b',' | b'}' if depth == 0 => break,
                    byte if depth == 0 && byte.is_ascii_whitespace() => break,
                    _ => {}
                }
            }
            end += 1;
        }
        output.replace_range(start..end, replacement);
    }
    Ok(output)
}
