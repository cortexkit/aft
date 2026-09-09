use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

static PROCESS_NONCE: OnceLock<[u8; 16]> = OnceLock::new();

/// Process-wide 128-bit CSPRNG process nonce.
fn get_process_nonce() -> [u8; 16] {
    *PROCESS_NONCE.get_or_init(|| {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).expect("failed to generate 128-bit CSPRNG process nonce");
        nonce
    })
}

/// An opaque generation token binding an index snapshot generation and a 128-bit CSPRNG
/// process nonce into a single opaque string.
///
/// Per the spec: equality is the only permitted operation. Substring, split, parse, order,
/// and range comparisons are strictly forbidden.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct GenerationToken {
    opaque: String,
}

impl GenerationToken {
    /// Construct a new opaque generation token from an index snapshot generation u64
    /// and the process-wide 128-bit CSPRNG process nonce.
    pub fn new(snapshot_generation: u64) -> Self {
        Self::new_with_str(&snapshot_generation.to_string())
    }

    /// Construct a new opaque generation token from an index snapshot generation string
    /// and the process-wide 128-bit CSPRNG process nonce.
    pub fn new_with_str(snapshot_generation: &str) -> Self {
        Self::new_with_nonce(snapshot_generation, get_process_nonce())
    }

    /// Construct a token with an explicit 128-bit nonce (useful for deterministic fixtures and testing).
    pub fn new_with_nonce(snapshot_generation: &str, nonce: [u8; 16]) -> Self {
        let nonce_hex = u128::from_be_bytes(nonce);
        let opaque = format!("{snapshot_generation}_{nonce_hex:032x}");
        Self { opaque }
    }

    /// Return the opaque token as a string slice for serialization and display.
    pub fn as_str(&self) -> &str {
        &self.opaque
    }
}

impl fmt::Debug for GenerationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GenerationToken({})", self.opaque)
    }
}

impl fmt::Display for GenerationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.opaque)
    }
}

impl Serialize for GenerationToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.opaque)
    }
}

impl<'de> Deserialize<'de> for GenerationToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self { opaque: s })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_and_uniqueness() {
        let t1 = GenerationToken::new(42);
        let t2 = GenerationToken::new(42);
        let t3 = GenerationToken::new(43);

        // Same generation within the same process has same process nonce
        assert_eq!(t1, t2);
        // Different generation produces different opaque token
        assert_ne!(t1, t3);

        // Custom nonces
        let n1 = [1u8; 16];
        let n2 = [2u8; 16];
        let tn1 = GenerationToken::new_with_nonce("42", n1);
        let tn2 = GenerationToken::new_with_nonce("42", n2);
        assert_ne!(tn1, tn2);
    }

    #[test]
    fn serialization_roundtrip() {
        let token = GenerationToken::new(100);
        let json = serde_json::to_string(&token).unwrap();
        let deserialized: GenerationToken = serde_json::from_str(&json).unwrap();
        assert_eq!(token, deserialized);
    }
}
