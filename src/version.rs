pub const BUILD_VERSION: &str = env!("KALISTO_BUILD_VERSION");

#[must_use]
pub fn build_version() -> &'static str {
    BUILD_VERSION
}
