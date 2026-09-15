//! Narrow codec boundary for temporal image storage.

#[cfg(feature = "vp9")]
pub mod vp9;
#[cfg(not(feature = "vp9"))]
#[path = "vp9_unavailable.rs"]
pub mod vp9;
