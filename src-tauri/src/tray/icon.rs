//! Status-colored tray icons.
//!
//! The four PNGs live in `src-tauri/icons/` and are embedded at compile time
//! so they're always available regardless of how the app is launched.

use tauri::image::Image;

const DISCONNECTED: &[u8] = include_bytes!("../../icons/tray-disconnected.png");
const IDLE: &[u8] = include_bytes!("../../icons/tray-idle.png");
const BUSY: &[u8] = include_bytes!("../../icons/tray-busy.png");
const ERROR: &[u8] = include_bytes!("../../icons/tray-error.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    Disconnected,
    Idle,
    Busy,
    Error,
}

impl IconKind {
    pub fn from_tag(tag: &str) -> Self {
        match tag {
            "mounted-idle" => Self::Idle,
            "mounted-busy" | "mounting" => Self::Busy,
            "error" => Self::Error,
            _ => Self::Disconnected,
        }
    }

    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::Disconnected => DISCONNECTED,
            Self::Idle => IDLE,
            Self::Busy => BUSY,
            Self::Error => ERROR,
        }
    }

    pub fn image(self) -> tauri::Result<Image<'static>> {
        Image::from_bytes(self.bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_tag_maps_known_states_to_distinct_icons() {
        assert_eq!(IconKind::from_tag("disconnected"), IconKind::Disconnected);
        assert_eq!(IconKind::from_tag("mounted-idle"), IconKind::Idle);
        assert_eq!(IconKind::from_tag("mounted-busy"), IconKind::Busy);
        assert_eq!(IconKind::from_tag("mounting"), IconKind::Busy);
        assert_eq!(IconKind::from_tag("error"), IconKind::Error);
    }

    #[test]
    fn from_tag_falls_back_to_disconnected_for_unknown_tags() {
        assert_eq!(IconKind::from_tag("anything-else"), IconKind::Disconnected);
    }

    #[test]
    fn each_icon_kind_has_a_non_empty_png_payload() {
        for kind in [
            IconKind::Disconnected,
            IconKind::Idle,
            IconKind::Busy,
            IconKind::Error,
        ] {
            let bytes = kind.bytes();
            assert!(bytes.len() > 50, "icon for {:?} too small", kind);
            // PNG signature
            assert_eq!(&bytes[..8], &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
        }
    }
}
