use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Opencode,
    Pi,
}

impl Harness {
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Opencode => "opencode",
            Harness::Pi => "pi",
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Harness {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "opencode" => Ok(Harness::Opencode),
            "pi" => Ok(Harness::Pi),
            other => Err(format!(
                "unsupported harness '{other}'; expected 'opencode' or 'pi'"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Harness;

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
}
