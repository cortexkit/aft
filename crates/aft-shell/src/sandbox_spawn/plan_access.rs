use super::*;

impl SpawnPlan {
    #[doc(hidden)]
    pub fn changed_files_read_profile(&self) -> Option<&SandboxProfile> {
        match self.policy() {
            Self::Launcher { profile, .. } => Some(profile),
            _ => None,
        }
    }

    pub fn refusal_code(&self) -> Option<&'static str> {
        match self.policy() {
            Self::Refused { code, .. } => Some(code),
            _ => None,
        }
    }

    pub fn refusal_message(&self) -> Option<&str> {
        match self.policy() {
            Self::Refused { message, .. } => Some(message),
            _ => None,
        }
    }
}
