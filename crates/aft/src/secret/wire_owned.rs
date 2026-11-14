pub use aft_base::secret::wire_owned::*;

#[cfg(test)]
#[allow(unused_imports)]
use super::wire::*;
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Deserializer};
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::{Zeroize, Zeroizing};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_credentials_are_wiped_on_success_and_every_decode_failure() {
        for suffix in [
            "",
            ",\"unknown\":true",
            ",\"code\":\"not_found\",\"message\":\"no\"",
        ] {
            let bytes = Zeroizing::new(format!("{{\"result\":{{\"status\":\"released\",\"kind\":\"agent_api_key\",\"key_b64url\":\"c2stZXhhbXBsZQ\",\"location\":\"authorization_bearer\"}}{suffix}}}"));
            WIPES.with(|n| n.set(0));
            let answer = parse(bytes.as_bytes());
            assert_eq!(answer.is_ok(), suffix.is_empty());
            assert!(WIPES.with(|n| n.get()) >= 4);
        }
        let bytes = br#"{"result":{"status":"released","kind":"agent_api_key","key_b64url":"not valid!","location":"authorization_bearer"}}"#;
        WIPES.with(|n| n.set(0));
        assert!(matches!(parse(bytes), Err(ReplyError::BadKeyEncoding)));
        assert!(WIPES.with(|n| n.get()) >= 4);
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    fn owned_decoder_reads_every_sender_reply_golden_and_refuses_contradictory_outer_envelopes() {
        for golden in super::super::wire_tests::USE_GOLDENS.lines().skip(3) {
            let bytes = Zeroizing::new(format!("{{\"result\":{golden}}}"));
            let SecretUseAnswer::Reply(reply) = parse(bytes.as_bytes()).unwrap() else {
                panic!("a documented sender reply must decode");
            };
            assert_eq!(serde_json::to_string(&reply).unwrap(), golden);
            for extra in [
                ",\"unexpected\":null",
                ",\"code\":\"invalid_params\",\"message\":\"invalid params\"",
                ",\"error\":{}",
            ] {
                let body = format!("{{\"result\":{golden}{extra}}}");
                assert!(parse(body.as_bytes()).is_err());
                assert!(
                    super::super::wire::parse_answer(&serde_json::from_str(&body).unwrap())
                        .is_err()
                );
            }
        }
    }
}

#[cfg(test)]
mod escaped_tests {
    use super::*;
    #[test]
    fn escaped_credential_text_decodes_directly_into_its_zeroizing_owner() {
        let bytes = Zeroizing::new(br#"{"result":{"status":"released","kind":"agent_api_key","key_b64url":"\u00632stZXhhbXBsZQ","location":"authorization_bearer"}}"#.to_vec());
        let SecretUseAnswer::Reply(SecretUseReply::ReleasedApiKey(key)) = parse(&bytes).unwrap()
        else {
            panic!("escaped JSON credential is still strict base64url");
        };
        assert_eq!(key.material(), b"sk-example");
        let text: Text = serde_json::from_slice(br#""\ud83d\ude00\u002f\t""#).unwrap();
        assert_eq!(text.as_str(), "😀/\t");
        WIPES.with(|n| n.set(0));
        assert!(parse(br#"{"result":{"key_b64url":"c2stZXhhbXBsZQ","status":"released","kind":"agent_api_key","location":42}}"#).is_err());
        assert!(
            WIPES.with(|n| n.get()) >= 3,
            "earlier parsed credential fields are wiped when a later field is malformed"
        );
    }
}

#[cfg(test)]
mod classified_error_tests {
    use super::*;
    #[test]
    fn owned_error_decode_retains_only_a_static_classification_and_wipes_peer_detail() {
        const KEY: &str = "peer-error-credential-0123456789";
        WIPES.with(|count| count.set(0));
        let payload = Zeroizing::new(format!(r#"{{"code":"invalid_params","message":"{KEY}"}}"#));
        let answer = parse(payload.as_bytes()).unwrap();
        let SecretUseAnswer::Rejected(error) = answer else {
            panic!("classified module error")
        };
        assert_eq!(error.code, "invalid_params");
        assert!(
            error.message.is_empty(),
            "peer detail is discarded rather than copied into an ordinary String"
        );
        assert!(!format!("{error:?}").contains(KEY));
        assert!(WIPES.with(|count| count.get()) >= 2);
        for payload in [
            format!(r#"{{"result":{{"error":{{"code":"{KEY}","class":"permanent"}}}}}}"#),
            format!(r#"{{"result":{{"error":{{"code":"not_found","class":"{KEY}"}}}}}}"#),
            format!(r#"{{"code":"{KEY}","message":"detail"}}"#),
        ] {
            let payload = Zeroizing::new(payload);
            let failure = parse(payload.as_bytes()).unwrap_err();
            assert!(!format!("{failure:?}").contains(KEY));
        }
    }

    #[test]
    fn owned_error_decode_preserves_every_documented_sender_refusal_classification() {
        for (code, class) in [
            ("not_found", "permanent"),
            ("limit_exceeded", "context_overflow"),
            ("secret_setup_incomplete", "permanent"),
            ("pending_limit", "transient"),
            ("store_error", "transient"),
        ] {
            let payload =
                format!(r#"{{"result":{{"error":{{"code":"{code}","class":"{class}"}}}}}}"#);
            let SecretUseAnswer::Refused(error) = parse(payload.as_bytes()).unwrap() else {
                panic!("sender refusal")
            };
            assert_eq!(error.code, code);
            assert_eq!(error.class.as_deref(), Some(class));
        }
    }
}
