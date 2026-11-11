//! Owned daemon reply parsing: encoded credentials are wiped even on refusals.

use super::wire::*;
use serde::{Deserialize, Deserializer};
use zeroize::{Zeroize, Zeroizing};

// Borrowing while deserializing from the zeroizing wire buffer avoids a second
// ordinary String owner before the credential enters its zeroizing owner.
struct Text(Zeroizing<String>);
impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // RawValue borrows the wire bytes without serde_json's ordinary decoded
        // string scratch buffer, including when the peer uses JSON escapes.
        let raw = <&serde_json::value::RawValue>::deserialize(d)?;
        let raw = raw.get();
        if !raw.starts_with('"') || !raw.ends_with('"') {
            return Err(serde::de::Error::custom("expected a string"));
        }
        let mut text = Zeroizing::new(String::with_capacity(raw.len() - 2));
        let mut chars = raw[1..raw.len() - 1].chars();
        fn hex(chars: &mut std::str::Chars<'_>) -> Option<u32> {
            (0..4).try_fold(0, |value, _| Some(value * 16 + chars.next()?.to_digit(16)?))
        }
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                text.push(ch);
                continue;
            }
            let decoded = match chars.next() {
                Some('"') => '"',
                Some('\\') => '\\',
                Some('/') => '/',
                Some('b') => '\u{0008}',
                Some('f') => '\u{000c}',
                Some('n') => '\n',
                Some('r') => '\r',
                Some('t') => '\t',
                Some('u') => {
                    let mut code = hex(&mut chars)
                        .ok_or_else(|| serde::de::Error::custom("invalid escape"))?;
                    if (0xd800..=0xdbff).contains(&code) {
                        if chars.next() != Some('\\') || chars.next() != Some('u') {
                            return Err(serde::de::Error::custom("invalid surrogate"));
                        }
                        let low = hex(&mut chars)
                            .filter(|low| (0xdc00..=0xdfff).contains(low))
                            .ok_or_else(|| serde::de::Error::custom("invalid surrogate"))?;
                        code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                    }
                    char::from_u32(code)
                        .ok_or_else(|| serde::de::Error::custom("invalid character"))?
                }
                _ => return Err(serde::de::Error::custom("invalid escape")),
            };
            text.push(decoded);
        }
        Ok(Text(text))
    }
}
impl Text {
    fn as_str(&self) -> &str {
        &self.0
    }
}
impl Drop for Text {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        WIPES.with(|n| n.set(n.get() + 1));
    }
}
#[cfg(test)]
thread_local! { static WIPES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    #[serde(default, deserialize_with = "present")]
    result: Option<ResultBody>,
    #[serde(default, deserialize_with = "present")]
    code: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    message: Option<Text>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Failure {
    code: Text,
    class: Option<Text>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ResultBody {
    #[serde(default, deserialize_with = "present")]
    status: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    kind: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    key_b64url: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    location: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    username: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    password: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    totp: Option<Option<Text>>,
    #[serde(default, deserialize_with = "present")]
    request_id: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    statement_b64url: Option<Text>,
    #[serde(default, deserialize_with = "present")]
    expires_at_ms: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    error: Option<Failure>,
}
fn malformed() -> ReplyError {
    ReplyError::Malformed {
        detail: "the reply does not match the contract".into(),
    }
}
impl ResultBody {
    fn count(&self) -> usize {
        [
            self.status.is_some(),
            self.kind.is_some(),
            self.key_b64url.is_some(),
            self.location.is_some(),
            self.username.is_some(),
            self.password.is_some(),
            self.totp.is_some(),
            self.request_id.is_some(),
            self.statement_b64url.is_some(),
            self.expires_at_ms.is_some(),
            self.error.is_some(),
        ]
        .into_iter()
        .filter(|v| *v)
        .count()
    }
    fn reply(self) -> Result<SecretUseAnswer, ReplyError> {
        let count = self.count();
        if let Some(error) = &self.error {
            if count != 1 {
                return Err(malformed());
            }
            return Ok(SecretUseAnswer::Refused(VaultError {
                code: error.code.as_str().into(),
                class: error.class.as_ref().map(|class| class.as_str().into()),
            }));
        }
        fn text(v: &Option<Text>) -> Result<&str, ReplyError> {
            v.as_ref().map(Text::as_str).ok_or_else(malformed)
        }
        let reply = match text(&self.status)? {
            "released" => match text(&self.kind)? {
                "agent_api_key" if count == 4 => {
                    SecretUseReply::ReleasedApiKey(ReleasedApiKey::new(
                        text(&self.location)?.into(),
                        super::vault::Material::new(super::wire::decode_key(text(
                            &self.key_b64url,
                        )?)?),
                    ))
                }
                "site_login" if count == 4 + usize::from(self.totp.is_some()) => {
                    SecretUseReply::ReleasedLogin(ReleasedLogin {
                        username: text(&self.username)?.into(),
                        password: super::vault::Material::new(
                            text(&self.password)?.as_bytes().to_vec(),
                        ),
                        totp: self
                            .totp
                            .as_ref()
                            .and_then(|v| v.as_ref())
                            .map(|v| Zeroizing::new(v.as_str().into())),
                    })
                }
                _ => return Err(malformed()),
            },
            "approval_required" if count == 4 => {
                let request_id = text(&self.request_id)?;
                let statement = text(&self.statement_b64url)?;
                if !super::wire::is_ulid(request_id) || super::wire::decode_key(statement).is_err()
                {
                    return Err(malformed());
                }
                SecretUseReply::ApprovalRequired(ApprovalRequest {
                    request_id: request_id.into(),
                    statement_b64url: statement.into(),
                    expires_at_ms: self.expires_at_ms.ok_or_else(malformed)?,
                })
            }
            _ => return Err(malformed()),
        };
        Ok(SecretUseAnswer::Reply(reply))
    }
}

pub(super) fn parse(bytes: &[u8]) -> Result<SecretUseAnswer, ReplyError> {
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| malformed())?;
    match (envelope.result, envelope.code, envelope.message) {
        (Some(result), None, None) => result.reply(),
        (None, Some(code), Some(message)) => Ok(SecretUseAnswer::Rejected(VaultTransportError {
            code: code.as_str().into(),
            message: message.as_str().into(),
        })),
        _ => Err(malformed()),
    }
}

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
