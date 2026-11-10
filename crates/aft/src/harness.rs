use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Harness {
    Opencode,
    Pi,
    Runner,
    Mcp { client: String },
    Fed { fingerprint: String },
}

impl Harness {
    pub fn storage_segment(&self) -> String {
        match self {
            Harness::Opencode => "opencode".to_string(),
            Harness::Pi => "pi".to_string(),
            Harness::Runner => "runner".to_string(),
            Harness::Mcp { client } => format!("mcp--{}", sanitize_client(client)),
            Harness::Fed { fingerprint } => format!(
                "fed--{}--{}",
                fingerprint
                    .get(..FED_SLUG_READABLE_HEX_LEN)
                    .unwrap_or(fingerprint),
                hash_hex_prefix(fingerprint, FED_SLUG_HASH_HEX_LEN)
            ),
        }
    }

    pub fn wire_label(&self) -> String {
        match self {
            Harness::Opencode => "opencode".to_string(),
            Harness::Pi => "pi".to_string(),
            Harness::Runner => "runner".to_string(),
            Harness::Mcp { client } => format!("mcp:{client}"),
            Harness::Fed { fingerprint } => format!("fed:{fingerprint}"),
        }
    }
}

/// Max length of the readable (pre-hash) slug portion. The full segment is
/// `mcp--<readable>--<32 hex>`, so the readable part is capped to keep directory
/// names bounded while the hash guarantees uniqueness.
const MCP_SLUG_READABLE_MAX: usize = 40;
const MCP_SLUG_HASH_HEX_LEN: usize = 32;
const FED_FINGERPRINT_MIN_HEX_LEN: usize = 32;
const FED_FINGERPRINT_MAX_HEX_LEN: usize = 64;
const FED_SLUG_READABLE_HEX_LEN: usize = 16;
const FED_SLUG_HASH_HEX_LEN: usize = 8;

fn hash_hex_prefix(raw: &str, hex_len: usize) -> String {
    let hash = blake3::hash(raw.as_bytes()).to_hex();
    hash.as_str()[..hex_len].to_string()
}

/// Build the storage slug for an MCP client. The readable portion is a
/// sanitized, length-capped rendering of the raw client; a short hash of the
/// RAW (un-sanitized) client is appended so that distinct clients that sanitize
/// to the same readable string (e.g. `a/b`, `a:b`, `a b`, casing variants, or
/// non-ASCII that collapses to `unknown`) still get distinct directories. The
/// hash is over the raw bytes, so it is collision-resistant where the readable
/// slug is not.
fn sanitize_client(client: &str) -> String {
    let lower = client.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_was_dash = false;
    for ch in lower.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-');
        if keep {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '.');
    let mut readable = if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    };
    if readable.len() > MCP_SLUG_READABLE_MAX {
        readable.truncate(MCP_SLUG_READABLE_MAX);
        // Truncation can leave a trailing separator; trim it for tidiness.
        readable = readable.trim_end_matches(['-', '.']).to_string();
        if readable.is_empty() {
            readable = "unknown".to_string();
        }
    }

    // A 128-bit hash suffix prevents hostile same-readable slugs from sharing
    // storage while keeping directory names short enough for common filesystems.
    format!(
        "{readable}--{}",
        hash_hex_prefix(client, MCP_SLUG_HASH_HEX_LEN)
    )
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_fed_harness(value: &str) -> Result<Harness, String> {
    let fingerprint = &value[4..];
    if !(FED_FINGERPRINT_MIN_HEX_LEN..=FED_FINGERPRINT_MAX_HEX_LEN).contains(&fingerprint.len())
        || !is_lower_hex(fingerprint)
    {
        return Err(format!(
            "unsupported harness '{value}'; fed fingerprint must be 32-64 lowercase hex characters"
        ));
    }
    Ok(Harness::Fed {
        fingerprint: fingerprint.to_string(),
    })
}

impl Serialize for Harness {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.wire_label())
    }
}

impl<'de> Deserialize<'de> for Harness {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HarnessVisitor;

        impl<'de> Visitor<'de> for HarnessVisitor {
            type Value = Harness;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "a harness string: 'opencode', 'pi', 'runner', 'mcp:<client>', or 'fed:<fingerprint>'",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Harness::from_str(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(HarnessVisitor)
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire_label())
    }
}

impl std::str::FromStr for Harness {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "opencode" => Ok(Harness::Opencode),
            "pi" => Ok(Harness::Pi),
            // `opencode2` names sessions that the Thalamus harness module runs
            // on an OpenCode 2 server, and `claude-code` the headless Claude
            // Code sessions it runs. Their AFT calls arrive over Thalamus's
            // own scoped routes, consent goes through core, and no AFT plugin
            // is loaded there, so they are runner sessions in everything the
            // harness decides (storage, consent, reply shape). The route
            // identity keeps the name the daemon sent.
            "runner" | "opencode2" | "claude-code" | "broca" => Ok(Harness::Runner),
            other if other.starts_with("mcp:") => {
                let client = &other[4..];
                if client.is_empty() {
                    Err(
                        "unsupported harness 'mcp:'; mcp client name must be non-empty".to_string(),
                    )
                } else {
                    Ok(Harness::Mcp {
                        client: client.to_string(),
                    })
                }
            }
            other if other.starts_with("fed:") => parse_fed_harness(other),
            other => Err(format!(
                "unsupported harness '{other}'; expected 'opencode', 'pi', 'runner', 'mcp:<client>', or 'fed:<fingerprint>'"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_client, Harness};

    #[test]
    fn directly_constructed_fed_storage_segment_handles_invalid_utf8_boundaries() {
        for fingerprint in ["short", "0123456789abcde€rest"] {
            let harness = Harness::Fed {
                fingerprint: fingerprint.to_string(),
            };
            let segment = harness.storage_segment();
            assert!(segment.starts_with("fed--"));
            assert_eq!(segment, harness.storage_segment());
        }
    }
    use std::str::FromStr;

    #[test]
    fn harness_enum_serde_roundtrip() {
        assert_eq!(
            serde_json::to_string(&Harness::Opencode).unwrap(),
            "\"opencode\""
        );
        assert_eq!(serde_json::to_string(&Harness::Pi).unwrap(), "\"pi\"");

        assert_eq!(
            serde_json::from_str::<Harness>("\"opencode\"").unwrap(),
            Harness::Opencode
        );
        assert_eq!(
            serde_json::from_str::<Harness>("\"pi\"").unwrap(),
            Harness::Pi
        );
        assert!(serde_json::from_str::<Harness>("\"claude_code\"").is_err());
    }

    #[test]
    fn opencode_pi_storage_segment_unchanged() {
        assert_eq!(Harness::Opencode.storage_segment(), "opencode");
        assert_eq!(Harness::Pi.storage_segment(), "pi");
    }

    #[test]
    fn claude_code_parses_as_the_runner_harness() {
        let harness = Harness::from_str("claude-code").unwrap();
        assert_eq!(harness, Harness::Runner);
        assert_eq!(harness.storage_segment(), "runner");
        // Only the exact name is an alias.
        for other in ["Claude-Code", "claude_code", "claude-code ", "claude"] {
            assert!(Harness::from_str(other).is_err(), "{other:?}");
        }
    }

    #[test]
    fn opencode2_parses_as_the_runner_harness() {
        let harness = Harness::from_str("opencode2").unwrap();
        assert_eq!(harness, Harness::Runner);
        assert_eq!(harness.storage_segment(), "runner");
        assert_eq!(
            serde_json::from_str::<Harness>("\"opencode2\"").unwrap(),
            Harness::Runner
        );
        // Only the exact name is an alias.
        for other in ["OpenCode2", "opencode 2", "opencode2 ", "opencode3"] {
            assert!(Harness::from_str(other).is_err(), "{other:?}");
        }
    }

    #[test]
    fn runner_round_trips() {
        assert_eq!(Harness::from_str("runner").unwrap(), Harness::Runner);
        assert_eq!(Harness::Runner.storage_segment(), "runner");
        assert_eq!(
            serde_json::to_string(&Harness::Runner).unwrap(),
            "\"runner\""
        );
        assert_eq!(
            serde_json::from_str::<Harness>("\"runner\"").unwrap(),
            Harness::Runner
        );
    }

    #[test]
    fn mcp_round_trips() {
        let h = Harness::Mcp {
            client: "claude-code".to_string(),
        };
        assert_eq!(serde_json::to_string(&h).unwrap(), "\"mcp:claude-code\"");
        assert_eq!(
            serde_json::from_str::<Harness>("\"mcp:claude-code\"").unwrap(),
            h
        );
        assert_eq!(
            Harness::from_str("mcp:cursor").unwrap(),
            Harness::Mcp {
                client: "cursor".to_string(),
            }
        );
        assert!(Harness::from_str("mcp:").is_err());
    }

    #[test]
    fn fed_round_trips_and_rejects_malformed_fingerprints() {
        let fingerprint64 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let h = Harness::Fed {
            fingerprint: fingerprint64.to_string(),
        };
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            format!("\"fed:{fingerprint64}\"")
        );
        assert_eq!(
            serde_json::from_str::<Harness>(&format!("\"fed:{fingerprint64}\"")).unwrap(),
            h
        );

        let fingerprint32 = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            Harness::from_str(&format!("fed:{fingerprint32}")).unwrap(),
            Harness::Fed {
                fingerprint: fingerprint32.to_string(),
            }
        );

        for invalid in [
            "fed:",
            "fed:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
            "fed:0123456789abcdef0123456789abcde",
            "fed:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
            "fed:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
            "fed:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef:extra",
        ] {
            assert!(
                Harness::from_str(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }
    }

    #[test]
    fn storage_segment_hostile_clients_are_path_safe() {
        let cases = ["../../etc", "a/b", r"a\b", "a:b", "", "Claude.Code"];
        for client in cases {
            let seg = Harness::Mcp {
                client: client.to_string(),
            }
            .storage_segment();
            assert!(
                !seg.is_empty(),
                "segment must be non-empty for client {client:?}"
            );
            assert!(
                !seg.contains(['/', '\\', ':']),
                "segment {seg:?} must not contain path separators for client {client:?}"
            );
            assert!(
                !seg.contains(".."),
                "segment {seg:?} must not contain '..' for client {client:?}"
            );
            assert!(
                seg.starts_with("mcp--"),
                "segment {seg:?} must use mcp-- prefix"
            );
        }
        // Readable portion preserved, hash suffix appended.
        let claude = Harness::Mcp {
            client: "Claude.Code".to_string(),
        }
        .storage_segment();
        assert!(
            claude.starts_with("mcp--claude.code--"),
            "expected readable slug with hash suffix, got {claude:?}"
        );
        // Empty client → readable "unknown" plus a (stable) hash of empty bytes.
        let empty = sanitize_client("");
        assert!(
            empty.starts_with("unknown--"),
            "empty client must render unknown-- plus hash, got {empty:?}"
        );
    }

    #[test]
    fn storage_segment_disambiguates_clients_that_sanitize_to_same_slug() {
        // a/b, a:b, a b, A-B all collapse to the readable slug "a-b" but are
        // DISTINCT clients — the raw-bytes hash suffix must keep their storage
        // directories distinct so two different MCP clients never share state.
        let seg = |c: &str| {
            Harness::Mcp {
                client: c.to_string(),
            }
            .storage_segment()
        };
        let variants = [seg("a/b"), seg("a:b"), seg("a b"), seg("A-B")];
        for s in &variants {
            assert!(
                s.starts_with("mcp--a-b--"),
                "expected shared readable slug a-b, got {s:?}"
            );
            let (_readable, suffix) = s.rsplit_once("--").expect("hash suffix");
            assert_eq!(
                suffix.len(),
                super::MCP_SLUG_HASH_HEX_LEN,
                "hash suffix must carry 128 bits of disambiguation: {s:?}"
            );
            assert!(
                suffix.chars().all(|ch| ch.is_ascii_hexdigit()),
                "hash suffix must be hex: {s:?}"
            );
        }
        let unique: std::collections::HashSet<_> = variants.iter().collect();
        assert_eq!(
            unique.len(),
            variants.len(),
            "distinct clients must get distinct storage segments: {variants:?}"
        );

        // Same raw client → same segment (deterministic, stable across calls).
        assert_eq!(seg("cursor"), seg("cursor"));

        // Very long client: readable portion is capped, segment stays bounded.
        let long = seg(&"x".repeat(500));
        assert!(
            long.len()
                <= "mcp--".len()
                    + super::MCP_SLUG_READABLE_MAX
                    + "--".len()
                    + super::MCP_SLUG_HASH_HEX_LEN,
            "long client segment must be length-bounded, got len {}",
            long.len()
        );
    }

    #[test]
    fn fed_storage_segment_is_path_safe_bounded_and_disambiguated() {
        let prefix = "0123456789abcdef";
        let fingerprint_a = format!("{prefix}{}", "0".repeat(48));
        let fingerprint_b = format!("{prefix}{}", "f".repeat(48));
        let seg = |fingerprint: &str| {
            Harness::Fed {
                fingerprint: fingerprint.to_string(),
            }
            .storage_segment()
        };
        let seg_a = seg(&fingerprint_a);
        let seg_b = seg(&fingerprint_b);
        for segment in [&seg_a, &seg_b] {
            assert!(
                segment.starts_with(&format!("fed--{prefix}--")),
                "fed segment must keep the 16-hex readable prefix, got {segment:?}"
            );
            assert!(
                !segment.contains(['/', '\\', ':']),
                "fed segment must be path-safe, got {segment:?}"
            );
            let (_readable, suffix) = segment.rsplit_once("--").expect("hash suffix");
            assert_eq!(
                suffix.len(),
                super::FED_SLUG_HASH_HEX_LEN,
                "fed hash suffix must use 8 hex chars, got {segment:?}"
            );
            assert!(
                suffix.chars().all(|ch| ch.is_ascii_hexdigit()),
                "fed hash suffix must be hex, got {segment:?}"
            );
            assert!(
                segment.len()
                    <= "fed--".len()
                        + super::FED_SLUG_READABLE_HEX_LEN
                        + "--".len()
                        + super::FED_SLUG_HASH_HEX_LEN,
                "fed segment must be length-bounded, got len {}",
                segment.len()
            );
        }
        assert_ne!(
            seg_a, seg_b,
            "distinct fed fingerprints that share a readable prefix must not share storage"
        );
        assert_eq!(seg(&fingerprint_a), seg_a);
    }
}
