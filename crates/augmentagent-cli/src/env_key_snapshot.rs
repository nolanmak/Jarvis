//! Compile-time fallback schema, independent of the config database/UI.
pub(super) const EMBEDDED_DOTENV_EXAMPLE: &str = include_str!("../../../.env.example");
