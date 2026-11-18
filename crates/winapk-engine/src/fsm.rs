//! Explicit VM lifecycle state machine — the replacement for upstream's
//! implicit-state async spaghetti. Every transition is validated; illegal
//! transitions are errors, not silent drift.

use std::fmt;

use winapk_core::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    /// No runtime / not provisioned.
    Off,
    Provisioning,
    /// Runtime + AVD exist, emulator not started.
    Provisioned,
    Launching,
    /// Process alive, Android not yet boot_completed.
    Booting,
    /// boot_completed=1, no app foreground.
    Ready,
    /// At least one app window attached.
    AppRunning,
    /// Graceful shutdown in flight.
    Stopping,
    /// Terminal-ish: requires reset() before reuse.
    Failed,
}

impl VmState {
    /// Legal transitions. `Failed` is reachable from everywhere (handled
    /// in `transition` directly, not via this table).
    fn allowed_from(self) -> &'static [VmState] {
        use VmState::*;
        match self {
            Off => &[Provisioning, Provisioned],
            Provisioning => &[Provisioned, Off],
            Provisioned => &[Launching, Provisioning],
            Launching => &[Booting, Failed],
            Booting => &[Ready, Stopping, Failed],
            Ready => &[AppRunning, Stopping],
            AppRunning => &[Ready, Stopping, Failed],
            Stopping => &[Off, Failed],
            Failed => &[Off],
        }
    }

    pub fn can_go(self, to: VmState) -> bool {
        to == VmState::Failed || self.allowed_from().contains(&to)
    }

    pub fn go(&mut self, to: VmState) -> Result<()> {
        if self.can_go(to) {
            *self = to;
            Ok(())
        } else {
            Err(Error::BadTransition {
                from: self.to_string(),
                to: to.to_string(),
            })
        }
    }
}

impl fmt::Display for VmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                VmState::Off => "off",
                VmState::Provisioning => "provisioning",
                VmState::Provisioned => "provisioned",
                VmState::Launching => "launching",
                VmState::Booting => "booting",
                VmState::Ready => "ready",
                VmState::AppRunning => "app-running",
                VmState::Stopping => "stopping",
                VmState::Failed => "failed",
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path() {
        let mut s = VmState::Off;
        s.go(VmState::Provisioning).unwrap();
        s.go(VmState::Provisioned).unwrap();
        s.go(VmState::Launching).unwrap();
        s.go(VmState::Booting).unwrap();
        s.go(VmState::Ready).unwrap();
        s.go(VmState::AppRunning).unwrap();
        s.go(VmState::Ready).unwrap();
        s.go(VmState::Stopping).unwrap();
        s.go(VmState::Off).unwrap();
    }

    #[test]
    fn reject_skip() {
        let mut s = VmState::Off;
        assert!(s.go(VmState::Ready).is_err());
    }

    #[test]
    fn fail_anywhere() {
        for st in [
            VmState::Off,
            VmState::Provisioned,
            VmState::Launching,
            VmState::Booting,
            VmState::Ready,
            VmState::AppRunning,
            VmState::Stopping,
        ] {
            let mut s = st;
            s.go(VmState::Failed).unwrap();
            assert_eq!(s, VmState::Failed);
            s.go(VmState::Off).unwrap();
        }
    }
}
