//! Owned daemon reply parsing: encoded credentials are wiped even on refusals.

use super::wire::*;
use serde::{Deserialize, Deserializer};
use zeroize::{Zeroize, Zeroizing};

// Borrowing while deserializing from the zeroizing wire buffer avoids a second
// ordinary String owner before the credential enters its zeroizing owner.
struct Text(Zeroizing<String>);
impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Text;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Text, E> {
                Ok(Text(Zeroizing::new(s.to_owned())))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Text, E> {
                Ok(Text(Zeroizing::new(s)))
            }
        }
        d.deserialize_string(Visitor)
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
    class: Text,
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
                class: error.class.as_str().into(),
            }));
        }
        let text = |v: &Option<Text>| v.as_ref().map(Text::as_str).ok_or_else(malformed);
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
                            .map(|v| v.as_str().into()),
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
