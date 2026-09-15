#![cfg_attr(not(unix), allow(unused))]
#[cfg(not(unix))]
compile_error!("Visual Store currently requires Linux or macOS.");

pub mod codec;
pub mod error;
mod filesystem;
pub mod image;
pub mod store;

pub use error::{Error, Result};
pub use store::{PutOptions, Store};

pub fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(bytes))
}

pub(crate) fn fault(point: &str) -> Result<()> {
    #[cfg(feature = "fault-injection")]
    {
        if std::env::var("VSTORE_TEST_CRASH").as_deref() == Ok(point) {
            // Abrupt termination deliberately bypasses destructors and SQLite cleanup.
            std::process::exit(91);
        }
        if std::env::var("VSTORE_TEST_DISK_FULL").as_deref() == Ok(point) {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
        }
    }
    let _ = point;
    Ok(())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
