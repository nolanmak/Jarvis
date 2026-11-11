//! Public key-only fallback input. Credential-prefixed paths stay excluded
//! from isolated source views; this ordinary source asset remains buildable.
//! Regenerate with `python3 crates/augmentagent-cli/tools/update_env_keys.py --write`.
pub(super) const EMBEDDED_DOTENV_EXAMPLE: &str = include_str!("env_keys.txt");
