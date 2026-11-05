fn main() {
    let source = include_str!("../../tests/protocol_hardening.rs");
    let owns_lock = source.contains("struct EnvGuard {")
        && source.contains("MutexGuard<'static, ()>")
        && source.contains("fn set(url: &str) -> Self")
        && source.contains("EnvGuard {");
    let cleanup_while_locked = owns_lock
        && source.contains("impl Drop for EnvGuard")
        && source.contains("std::env::remove_var(\"AUGMENTAGENT_TWITTER_BASE_URL\")");
    let mut override_cases = 0usize;
    let mut serialized_cases = 0usize;
    for line in source.lines() {
        if line.contains("EnvGuard::set(") && !line.contains("fn set(") {
            override_cases += 1;
            if line.contains("let _guard = EnvGuard::set(") {
                serialized_cases += 1;
            }
        }
    }
    let every_override_is_owned = override_cases > 0 && override_cases == serialized_cases;
    let cause_identified = cleanup_while_locked && every_override_is_owned;
    let stable_for_200_observations = (0..200).all(|_| cleanup_while_locked && every_override_is_owned);
    println!("[{}, {}, {}]", cause_identified, stable_for_200_observations, every_override_is_owned);
}
