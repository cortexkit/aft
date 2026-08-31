use crate::config::IndexResourcePolicy;

pub const HEALTHY_SAMPLES_TO_RESUME: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    External,
    Battery,
    BatterySaving,
    NoBattery,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalState {
    Healthy,
    High,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSnapshot {
    pub power: PowerState,
    pub cpu_pressure: SignalState,
    pub memory_pressure: SignalState,
    pub io_pressure: SignalState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    BatterySaving,
    CpuPressure,
    MemoryPressure,
    IoPressure,
    UnknownPower,
    UnknownCpuPressure,
    UnknownMemoryPressure,
    UnknownIoPressure,
    Recovering,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    Admit,
    Paused(PauseReason),
}

#[derive(Debug, Default)]
pub struct ResourceAdmissionGate {
    paused: bool,
    healthy_samples: u8,
}

impl ResourceAdmissionGate {
    pub fn observe(
        &mut self,
        policy: IndexResourcePolicy,
        snapshot: ResourceSnapshot,
    ) -> AdmissionDecision {
        if policy == IndexResourcePolicy::Performance {
            self.paused = false;
            self.healthy_samples = 0;
            return AdmissionDecision::Admit;
        }

        if let Some(reason) = pause_reason(snapshot) {
            self.paused = true;
            self.healthy_samples = 0;
            return AdmissionDecision::Paused(reason);
        }

        if !self.paused {
            return AdmissionDecision::Admit;
        }

        self.healthy_samples = self.healthy_samples.saturating_add(1);
        if self.healthy_samples >= HEALTHY_SAMPLES_TO_RESUME {
            self.paused = false;
            self.healthy_samples = 0;
            AdmissionDecision::Admit
        } else {
            AdmissionDecision::Paused(PauseReason::Recovering)
        }
    }
}

fn pause_reason(snapshot: ResourceSnapshot) -> Option<PauseReason> {
    match snapshot.power {
        PowerState::BatterySaving => return Some(PauseReason::BatterySaving),
        PowerState::Unknown => return Some(PauseReason::UnknownPower),
        PowerState::External | PowerState::Battery | PowerState::NoBattery => {}
    }
    match snapshot.memory_pressure {
        SignalState::High => return Some(PauseReason::MemoryPressure),
        SignalState::Unknown => return Some(PauseReason::UnknownMemoryPressure),
        SignalState::Healthy => {}
    }
    match snapshot.io_pressure {
        SignalState::High => return Some(PauseReason::IoPressure),
        SignalState::Unknown => return Some(PauseReason::UnknownIoPressure),
        SignalState::Healthy => {}
    }
    match snapshot.cpu_pressure {
        SignalState::High => Some(PauseReason::CpuPressure),
        SignalState::Unknown => Some(PauseReason::UnknownCpuPressure),
        SignalState::Healthy => None,
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_psi(input: &str, kind: PressureKind) -> SignalState {
    let prefix = match kind {
        PressureKind::Cpu => "some ",
        PressureKind::Stall => "full ",
    };
    let Some(line) = input.lines().find(|line| line.starts_with(prefix)) else {
        return SignalState::Unknown;
    };
    let Some(avg10) = line
        .split_ascii_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))
        .and_then(|value| value.parse::<f64>().ok())
    else {
        return SignalState::Unknown;
    };
    if avg10 > 0.0 {
        SignalState::High
    } else {
        SignalState::Healthy
    }
}
#[cfg(target_os = "linux")]
fn sample_linux_power_at(root: &std::path::Path) -> PowerState {
    let Ok(entries) = std::fs::read_dir(root) else {
        return PowerState::Unknown;
    };
    let mut battery_capacity = None;
    let mut found_battery = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type"))
            .ok()
            .map(|value| value.trim().to_owned());
        match kind.as_deref() {
            Some("Mains" | "USB" | "USB_C" | "USB_PD") => {
                if std::fs::read_to_string(path.join("online"))
                    .ok()
                    .is_some_and(|value| value.trim() == "1")
                {
                    return PowerState::External;
                }
            }
            Some("Battery") => {
                found_battery = true;
                if let Ok(value) = std::fs::read_to_string(path.join("capacity")) {
                    battery_capacity = value.trim().parse::<u8>().ok().or(battery_capacity);
                }
            }
            _ => {}
        }
    }
    if !found_battery {
        PowerState::NoBattery
    } else if battery_capacity.is_some_and(|capacity| capacity <= 10) {
        PowerState::BatterySaving
    } else {
        PowerState::Battery
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
enum PressureKind {
    Cpu,
    Stall,
}

pub fn sample_resources() -> ResourceSnapshot {
    platform::sample()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub(super) fn sample() -> ResourceSnapshot {
        let pressure = |path: &str, kind| {
            std::fs::read_to_string(path)
                .ok()
                .map_or(SignalState::Unknown, |value| parse_linux_psi(&value, kind))
        };
        ResourceSnapshot {
            power: sample_linux_power_at(std::path::Path::new("/sys/class/power_supply")),
            cpu_pressure: pressure("/proc/pressure/cpu", PressureKind::Cpu),
            memory_pressure: pressure("/proc/pressure/memory", PressureKind::Stall),
            io_pressure: pressure("/proc/pressure/io", PressureKind::Stall),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::*;

    pub(super) fn sample() -> ResourceSnapshot {
        ResourceSnapshot {
            power: PowerState::Unknown,
            cpu_pressure: SignalState::Unknown,
            memory_pressure: SignalState::Unknown,
            io_pressure: SignalState::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> ResourceSnapshot {
        ResourceSnapshot {
            power: PowerState::External,
            cpu_pressure: SignalState::Healthy,
            memory_pressure: SignalState::Healthy,
            io_pressure: SignalState::Healthy,
        }
    }

    #[test]
    fn balanced_pauses_on_battery_saving_and_pressure() {
        let mut gate = ResourceAdmissionGate::default();
        let mut battery = healthy();
        battery.power = PowerState::BatterySaving;
        assert_eq!(
            gate.observe(IndexResourcePolicy::Balanced, battery),
            AdmissionDecision::Paused(PauseReason::BatterySaving)
        );

        let mut pressured = healthy();
        pressured.memory_pressure = SignalState::High;
        assert_eq!(
            gate.observe(IndexResourcePolicy::Balanced, pressured),
            AdmissionDecision::Paused(PauseReason::MemoryPressure)
        );
    }

    #[test]
    fn balanced_reports_unknown_portable_pressure_conservatively() {
        let mut gate = ResourceAdmissionGate::default();
        let mut unknown = healthy();
        unknown.io_pressure = SignalState::Unknown;
        assert_eq!(
            gate.observe(IndexResourcePolicy::Balanced, unknown),
            AdmissionDecision::Paused(PauseReason::UnknownIoPressure)
        );
    }

    #[test]
    fn desktop_without_battery_is_not_treated_as_battery_powered() {
        let mut gate = ResourceAdmissionGate::default();
        let mut desktop = healthy();
        desktop.power = PowerState::NoBattery;
        for _ in 0..HEALTHY_SAMPLES_TO_RESUME {
            gate.observe(IndexResourcePolicy::Balanced, desktop);
        }
        assert_eq!(
            gate.observe(IndexResourcePolicy::Balanced, desktop),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn balanced_requires_consecutive_healthy_samples_after_pause() {
        let mut gate = ResourceAdmissionGate::default();
        let mut pressured = healthy();
        pressured.io_pressure = SignalState::High;
        assert!(matches!(
            gate.observe(IndexResourcePolicy::Balanced, pressured),
            AdmissionDecision::Paused(PauseReason::IoPressure)
        ));

        for _ in 1..HEALTHY_SAMPLES_TO_RESUME {
            assert_eq!(
                gate.observe(IndexResourcePolicy::Balanced, healthy()),
                AdmissionDecision::Paused(PauseReason::Recovering)
            );
        }
        assert_eq!(
            gate.observe(IndexResourcePolicy::Balanced, healthy()),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn unhealthy_sample_resets_resume_hysteresis() {
        let mut gate = ResourceAdmissionGate::default();
        let mut pressured = healthy();
        pressured.cpu_pressure = SignalState::High;
        gate.observe(IndexResourcePolicy::Balanced, pressured);
        gate.observe(IndexResourcePolicy::Balanced, healthy());
        gate.observe(IndexResourcePolicy::Balanced, pressured);

        for _ in 1..HEALTHY_SAMPLES_TO_RESUME {
            assert_eq!(
                gate.observe(IndexResourcePolicy::Balanced, healthy()),
                AdmissionDecision::Paused(PauseReason::Recovering)
            );
        }
    }

    #[test]
    fn performance_bypasses_resource_admission_only() {
        let mut gate = ResourceAdmissionGate::default();
        let snapshot = ResourceSnapshot {
            power: PowerState::BatterySaving,
            cpu_pressure: SignalState::High,
            memory_pressure: SignalState::High,
            io_pressure: SignalState::High,
        };
        assert_eq!(
            gate.observe(IndexResourcePolicy::Performance, snapshot),
            AdmissionDecision::Admit
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_psi_uses_cpu_some_and_full_stall_pressure() {
        let healthy = "some avg10=0.00 avg60=1.00 avg300=2.00 total=10\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        let cpu_high = "some avg10=0.01 avg60=0.00 avg300=0.00 total=10\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        let stall_high = "some avg10=2.00 avg60=1.00 avg300=2.00 total=10\nfull avg10=0.01 avg60=0.00 avg300=0.00 total=1\n";
        assert_eq!(
            parse_linux_psi(healthy, PressureKind::Cpu),
            SignalState::Healthy
        );
        assert_eq!(
            parse_linux_psi(cpu_high, PressureKind::Cpu),
            SignalState::High
        );
        assert_eq!(
            parse_linux_psi(healthy, PressureKind::Stall),
            SignalState::Healthy
        );
        assert_eq!(
            parse_linux_psi(stall_high, PressureKind::Stall),
            SignalState::High
        );
        assert_eq!(
            parse_linux_psi("garbled", PressureKind::Cpu),
            SignalState::Unknown
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_power_sampler_distinguishes_ac_battery_saver_and_desktop() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sample_linux_power_at(dir.path()), PowerState::NoBattery);

        let ac = dir.path().join("AC");
        std::fs::create_dir(&ac).unwrap();
        std::fs::write(ac.join("type"), "Mains\n").unwrap();
        std::fs::write(ac.join("online"), "1\n").unwrap();
        assert_eq!(sample_linux_power_at(dir.path()), PowerState::External);

        std::fs::write(ac.join("online"), "0\n").unwrap();
        let battery = dir.path().join("BAT0");
        std::fs::create_dir(&battery).unwrap();
        std::fs::write(battery.join("type"), "Battery\n").unwrap();
        std::fs::write(battery.join("capacity"), "80\n").unwrap();
        assert_eq!(sample_linux_power_at(dir.path()), PowerState::Battery);

        std::fs::write(battery.join("capacity"), "5\n").unwrap();
        assert_eq!(sample_linux_power_at(dir.path()), PowerState::BatterySaving);
    }
}
