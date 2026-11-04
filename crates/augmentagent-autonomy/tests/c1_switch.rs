//! #1833 C1 — D1 master switch. Its own test binary: it mutates the process
//! environment, so nothing else may run in parallel with it.

use augmentagent_autonomy::{
    heartbeat_outreach_from_env, master_switch_from_env, parse_switch, SwitchValue,
};

#[test]
fn autonomy_env_only_on_enables() {
    for on in ["on", "ON", " On ", "\ton\n"] {
        assert_eq!(parse_switch(Some(on)), SwitchValue::On, "{on:?}");
    }
    for off in [None, Some(""), Some("  "), Some("off"), Some("OFF")] {
        assert_eq!(parse_switch(off), SwitchValue::Off, "{off:?}");
    }
    for typo in ["true", "1", "yes", "enabled", "onn", "o n"] {
        let v = parse_switch(Some(typo));
        assert_eq!(v, SwitchValue::Unrecognised, "{typo:?}");
        assert!(!v.enabled(), "{typo:?} must not enable");
    }

    std::env::remove_var("AUGMENTAGENT_AUTONOMY");
    std::env::remove_var("AUGMENTAGENT_HEARTBEAT_OUTREACH");
    assert!(!master_switch_from_env());
    std::env::set_var("AUGMENTAGENT_AUTONOMY", "true");
    assert!(!master_switch_from_env());
    std::env::set_var("AUGMENTAGENT_AUTONOMY", " On ");
    assert!(master_switch_from_env());

    // D16: outreach needs both switches.
    assert!(!heartbeat_outreach_from_env());
    std::env::set_var("AUGMENTAGENT_HEARTBEAT_OUTREACH", "on");
    assert!(heartbeat_outreach_from_env());
    std::env::set_var("AUGMENTAGENT_AUTONOMY", "off");
    assert!(!heartbeat_outreach_from_env());
}
