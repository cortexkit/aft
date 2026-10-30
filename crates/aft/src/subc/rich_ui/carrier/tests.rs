use super::*;

fn entry(id: &str) -> Value {
    operations::operations()
        .into_iter()
        .find(|entry| entry["id"] == id)
        .unwrap()
}

#[test]
fn carrier_error_projection_drops_canaries_and_oversized_data() {
    for code in ["task_not_found", "path_outside_root", "unlisted_code"] {
        let response = Response::error_with_data(
            "canary-id",
            code,
            "canary-message",
            json!({
                "payload":"canary-data".repeat(200_000), "status_bar":"canary-status", "bg_completions":["canary-background"]
            }),
        );
        let body = response_body(&entry("read"), &response);
        assert_eq!(
            body,
            json!({"content":[{"type":"text","text":code}], "isError":true,
            "structuredContent":{"success":false,"code":code,"message":operations::fixed_message(code)}})
        );
        validate_body(None, &body).unwrap();
        assert!(!body.to_string().contains("canary"));
    }
}

#[test]
fn carrier_success_projection_validation_and_exact_answer_cap() {
    let entry = entry("read");
    let response = Response::success(
        "canary-id",
        json!({"text":"hello", "canary":"drop", "view":{}, "status_bar":"drop"}),
    );
    let body = response_body(&entry, &response);
    assert_eq!(
        body,
        json!({"content":[{"type":"text","text":""}],"isError":false,"structuredContent":{"text":"hello"}})
    );
    validate_body(Some(&entry), &body).unwrap();
    for data in [
        json!({"canary":"missing"}),
        json!({"text":42,"canary":"wrong"}),
    ] {
        assert_eq!(
            response_body(&entry, &Response::success("id", data)),
            error_body("result_schema_mismatch")
        );
    }
    let overhead = serde_json::to_vec(&json!({"text":""})).unwrap().len();
    for extra in [0, 1] {
        let response = Response::success(
            "id",
            json!({"text":"x".repeat(ANSWER_BYTES-overhead+extra)}),
        );
        assert_eq!(
            serde_json::to_vec(&response.data).unwrap().len(),
            ANSWER_BYTES + extra
        );
        let body = response_body(&entry, &response);
        validate_body(Some(&entry), &body).unwrap();
        if extra == 0 {
            assert_eq!(body["isError"], false);
        } else {
            assert_eq!(body, error_body("answer_too_large"));
        }
    }
}

#[test]
fn carrier_validator_checks_exact_body_and_page_shape() {
    let entry = entry("bash_output");
    let body = response_body(
        &entry,
        &Response::success(
            "id",
            json!({"items":[],"next":null,"total":99,"canary":"drop"}),
        ),
    );
    assert_eq!(body["structuredContent"], json!({"items":[],"next":null}));
    validate_body(Some(&entry), &body).unwrap();
    for malformed in [
        json!({"content":[{"type":"text","text":""}],"isError":false,"structuredContent":{"items":[],"next":null,"success":true}}),
        json!({"content":[],"isError":false,"structuredContent":{"items":[],"next":null}}),
        json!({"content":[{"type":"text","text":""}],"isError":false,"structuredContent":{"items":[],"next":null},"id":"bad"}),
        json!({"content":[{"type":"text","text":"args_invalid"}],"isError":true,"structuredContent":{"success":false,"code":"args_invalid","message":"canary"}}),
    ] {
        assert!(validate_body(Some(&entry), &malformed).is_err());
    }
}
