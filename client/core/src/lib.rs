//! Ядро клиента Feather: всё, что не зависит от интерфейса.
//! UI (Slint) вызывает методы [`Api`] и получает события шлюза через колбэк.

pub mod api;
pub mod gateway;

pub use api::{Api, Error};
pub use gateway::{GatewayEvent, GatewayHandle};
