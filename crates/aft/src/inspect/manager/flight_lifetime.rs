use super::*;

impl Drop for Tier2FlightExitGuard<'_> {
    fn drop(&mut self) {
        if let Some(identity) = &self.identity {
            self.manager.finish_tier2_flight_if_current(
                &self.key,
                Some(identity),
                JobOutcome::Failed {
                    message: "tier2 reuse worker exited without publishing a result".to_string(),
                },
            );
        }
    }
}
