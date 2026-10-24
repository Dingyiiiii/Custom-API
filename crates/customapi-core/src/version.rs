//! Build identity for every customs binary.

use std::fmt;

/// Version is overridden at build time with the CUSTOMS_VERSION environment
/// variable; the default marks a development build.
pub const VERSION: &str = match option_env!("CUSTOMS_VERSION") {
    Some(v) => v,
    None => "0.1.0-dev",
};

/// Commit is the git revision the binary was built from.
pub const COMMIT: &str = match option_env!("CUSTOMS_COMMIT") {
    Some(v) => v,
    None => "unknown",
};

/// BUILD_TIME is when the binary was produced.
pub const BUILD_TIME: &str = match option_env!("CUSTOMS_BUILD_TIME") {
    Some(v) => v,
    None => "unknown",
};

/// Identity is the rendered build identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Semantic version.
    pub version: String,
    /// Git revision.
    pub commit: String,
    /// Build timestamp.
    pub build_time: String,
}

impl Identity {
    /// Returns the build identity of the running binary.
    pub fn current() -> Self {
        Identity {
            version: VERSION.to_string(),
            commit: COMMIT.to_string(),
            build_time: BUILD_TIME.to_string(),
        }
    }
}

impl fmt::Display for Identity {
    /// Renders a one-line build identity, mirroring the Go `version.String()`:
    /// `customapi 0.1.0 (commit abc, built now, freebsd/amd64, rustc 1.99)`.
    ///
    /// The architecture is spelled the way Go spells it (amd64, arm64) so that
    /// operators and scripts see the same identity whichever binary produced
    /// it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "customapi {} (commit {}, built {}, {}/{}, rustc {})",
            self.version,
            self.commit,
            self.build_time,
            std::env::consts::OS,
            go_arch_name(std::env::consts::ARCH),
            option_env!("CUSTOMS_RUSTC").unwrap_or("unknown"),
        )
    }
}

/// Maps Rust's architecture names onto Go's `GOARCH` spellings.
fn go_arch_name(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_names_the_go_architecture() {
        assert_eq!(go_arch_name("x86_64"), "amd64");
        assert_eq!(go_arch_name("aarch64"), "arm64");
        assert_eq!(go_arch_name("riscv64"), "riscv64");
        let line = identity();
        assert!(line.starts_with("customapi "), "{line}");
        assert!(line.contains("commit "), "{line}");
        assert!(line.contains("rustc "), "{line}");
    }
}

/// Returns the one-line build identity, mirroring `version.String()`.
pub fn identity() -> String {
    Identity::current().to_string()
}
