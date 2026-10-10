//! Ядро клиента Feather: всё, что не зависит от интерфейса.
//! UI (Slint) вызывает методы [`Api`] и получает события шлюза через колбэк.

pub mod api;
pub mod gateway;
pub mod hotkey;
pub mod voice;

pub use api::{Api, Error};
pub use gateway::{GatewayEvent, GatewayHandle};
pub use hotkey::{HotkeyEvent, HotkeyHandle};
pub use voice::{VoiceEvent, VoiceOptions, VoicePeer, VoiceSession, list_devices};
