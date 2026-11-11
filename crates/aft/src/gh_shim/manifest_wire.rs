//! The three management-surface operations admitted to `reserved:aft`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    pub key_id: String,
    pub manifest_bytes: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Latest {
    pub version: u64,
    pub envelope: Envelope,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Report {
    pub version: u64,
    pub payload_sha256: String,
    pub verdict: String,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Recorded {
    pub status: String,
    pub member_id: String,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Notice {
    pub kind: String,
    pub epoch: String,
    pub seq: u64,
    pub version: Option<u64>,
    pub manifest_sha256: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Refusal {
    pub code: String,
    pub class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy_member: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum ResultBody<T> {
    Refused { error: Refusal },
    Success(T),
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Reply<T> {
    pub result: ResultBody<T>,
}

#[cfg(test)]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModuleError {
    code: String,
    message: String,
}

pub(super) fn latest_request() -> Value {
    json!({"method": "manifest.latest", "params": {}})
}

pub(super) fn watch_request() -> Value {
    json!({"method": "manifest.watch", "params": {}})
}

pub(super) fn report_request(report: &Report) -> Value {
    json!({"method": "manifest.report_active", "params": report})
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn manifest_sync_vectors_pin_requests_replies_refusals_and_notices() {
        let bytes = include_bytes!("../../../../test-vectors/manifest-binding-v1.json");
        assert_eq!(
            format!("{:x}", Sha256::digest(bytes)),
            "8c070db5272be78e6682714a53628453e5bf7fda5a43431d4e73b4d663218433"
        );
        let fixture: Value = serde_json::from_slice(bytes).unwrap();
        let samples = fixture["samples"].as_object().unwrap();
        assert_eq!(samples.len(), 45);
        fn round_trip<T: serde::de::DeserializeOwned + Serialize>(value: &Value) {
            let decoded: T = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(
                serde_json::to_vec(&serde_json::to_value(decoded).unwrap()).unwrap(),
                serde_json::to_vec(value).unwrap()
            );
        }
        round_trip::<Reply<Latest>>(&samples["latest.reply"]);
        let latest: Reply<Latest> =
            serde_json::from_value(samples["latest.reply"].clone()).unwrap();
        let ResultBody::Success(latest) = latest.result else {
            panic!("latest refused")
        };
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(latest.envelope.manifest_bytes.as_bytes())
            ),
            samples["watch.stored"]["manifest_sha256"].as_str().unwrap()
        );
        round_trip::<Reply<Recorded>>(&samples["report.reply"]);
        let report: Report =
            serde_json::from_value(samples["report.request"]["params"].clone()).unwrap();
        assert_eq!(
            serde_json::to_vec(&report_request(&report)).unwrap(),
            serde_json::to_vec(&samples["report.request"]).unwrap()
        );
        for name in ["watch.empty", "watch.stored"] {
            round_trip::<Notice>(&samples[name]);
        }
        for name in [
            "refusal.home_unavailable",
            "refusal.not_found",
            "refusal.remote_setup_incomplete",
            "refusal.remote_setup_incomplete.forwarded",
            "refusal.remote_setup_incomplete.forwarded_with_remedy",
            "refusal.remote_setup_incomplete.with_remedy",
            "refusal.secret_setup_incomplete",
            "refusal.store_error",
        ] {
            round_trip::<Reply<Latest>>(&samples[name]);
            round_trip::<Reply<Recorded>>(&samples[name]);
            round_trip::<Reply<Value>>(&samples[name]);
        }
        round_trip::<ModuleError>(&samples["refusal.invalid_params"]);
        assert_eq!(samples["refusal.invalid_params"]["code"], "invalid_params");
        assert!(samples["refusal.invalid_params"].get("class").is_none());
        assert_eq!(
            serde_json::to_vec(&latest_request()).unwrap(),
            br#"{"method":"manifest.latest","params":{}}"#
        );
        assert_eq!(
            serde_json::to_vec(&watch_request()).unwrap(),
            br#"{"method":"manifest.watch","params":{}}"#
        );
    }
}
