//! Tray-state derivation and tooltip strings.
//!
//! S07 specifies four states with explicit human-readable tooltips. The
//! 2-second stabilization debouncer is implemented as a deadline that the
//! tray loop applies before transitioning back to idle.

use std::time::{Duration, Instant};

use crate::state::{AppState, MountState};

pub const STABILIZE_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayState {
    Disconnected,
    MountedIdle,
    MountedBusy,
    Error(String),
}

impl TrayState {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::MountedIdle => "mounted-idle",
            Self::MountedBusy => "mounted-busy",
            Self::Error(_) => "error",
        }
    }

    pub fn tooltip(&self) -> String {
        match self {
            Self::Disconnected => "BRAIN disconnected".into(),
            Self::MountedIdle => "BRAIN ready – safe to remove".into(),
            Self::MountedBusy => "BRAIN busy – do not remove".into(),
            Self::Error(msg) => format!("BRAIN error – {msg}"),
        }
    }
}

/// Derives the tray state from the current AppState. The `last_busy_at`
/// argument tracks when the active-op counter last hit zero — a transition
/// to `MountedIdle` only happens once `STABILIZE_WINDOW` has passed.
pub fn derive(app: &AppState, last_busy_at: Option<Instant>, now: Instant) -> TrayState {
    match app.mount() {
        MountState::Disconnected => TrayState::Disconnected,
        MountState::Mounting => TrayState::MountedBusy,
        MountState::Error(msg) => TrayState::Error(msg),
        MountState::MountedIdle | MountState::MountedBusy => {
            if app.active_ops() > 0 {
                return TrayState::MountedBusy;
            }
            match last_busy_at {
                Some(t) if now.duration_since(t) < STABILIZE_WINDOW => TrayState::MountedBusy,
                _ => TrayState::MountedIdle,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_app_state_yields_disconnected_tray() {
        let app = AppState::new();
        let state = derive(&app, None, Instant::now());
        assert_eq!(state, TrayState::Disconnected);
    }

    #[test]
    fn busy_state_when_any_active_op_running_independent_of_timing() {
        let app = AppState::new();
        app.set_mount(MountState::MountedIdle);
        app.begin_op("test");
        let state = derive(&app, Some(Instant::now()), Instant::now());
        assert_eq!(state, TrayState::MountedBusy);
    }

    #[test]
    fn busy_to_idle_requires_two_seconds_of_inactivity() {
        let app = AppState::new();
        app.set_mount(MountState::MountedIdle);
        // simulate a recent busy window that just ended
        let now = Instant::now();
        let state = derive(&app, Some(now), now + Duration::from_millis(500));
        assert_eq!(state, TrayState::MountedBusy);
        let state = derive(&app, Some(now), now + Duration::from_secs(3));
        assert_eq!(state, TrayState::MountedIdle);
    }

    #[test]
    fn tooltip_strings_use_human_language_for_each_state() {
        assert!(TrayState::Disconnected.tooltip().contains("disconnected"));
        assert!(TrayState::MountedIdle.tooltip().contains("safe to remove"));
        assert!(TrayState::MountedBusy.tooltip().contains("do not remove"));
        assert!(TrayState::Error("boom".into()).tooltip().contains("boom"));
    }

    #[test]
    fn error_state_propagates_message_through_to_tray_state() {
        let app = AppState::new();
        app.set_mount(MountState::Error("disk gone".into()));
        let s = derive(&app, None, Instant::now());
        assert!(matches!(s, TrayState::Error(_)));
    }
}
