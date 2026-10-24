//! Helpers shared by the integration test binaries.

use sshweave::config::Config;
use std::{path::Path, sync::OnceLock};

/// Returns an empty directory used as `HOME` for the whole test binary.
///
/// The default file `~/.config/sshweave/sshweave.toml` implicitly includes
/// conf.d, and `~` expands against `HOME`. Tests must not see the developer's
/// real home directory.
pub(crate) fn isolated_home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = tempfile::Builder::new()
            .prefix("sshweave-home-")
            .tempdir()
            .expect("create isolated HOME");
        // SAFETY: every test calls this before reading the environment, and
        // `OnceLock` blocks concurrent callers until the update completes.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("HOME", home.path());
        }
        home
    })
    .path()
}

/// Loads a configuration with `HOME` isolated.
pub(crate) fn load(path: impl AsRef<Path>) -> anyhow::Result<Config> {
    isolated_home();
    Config::load(path)
}
