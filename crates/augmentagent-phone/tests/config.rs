use augmentagent_phone::config::Settings;
use std::collections::HashMap;
fn values() -> HashMap<&'static str, String> {
    [
    ("AUGMENTAGENT_PHONE_ENABLED","1"),
    ("AUGMENTAGENT_PHONE_PUBLIC_URL","https://phone.example.test"),
    ("AUGMENTAGENT_PHONE_BIND","127.0.0.1:8878"),
    ("AUGMENTAGENT_PHONE_VERIFICATION_KEY","00000000000000000000000000000000"),
    ("TWILIO_ACCOUNT_SID","AC00000000000000000000000000000000"),
    ("TWILIO_AUTH_TOKEN","test-token"),
    ("TWILIO_PHONE_NUMBER","+12025550100"),
    ("AUGMENTAGENT_PHONE_OWNER_LINKS",r#"[{"platform":"slack","account":"T-fixture","surface_owner":"U-fixture","owner":"canonical-owner"},{"platform":"discord","account":"guild-fixture","surface_owner":"user-fixture","owner":"canonical-owner"}]"#),
].into_iter().map(|(k,v)|(k,v.to_owned())).collect()
}
#[test]
fn ac01_configuration_defaults_off_and_requires_complete_explicit_setup() {
    assert!(Settings::from_lookup(|_| None).unwrap().is_none());
    let mut env = values();
    env.insert("AUGMENTAGENT_PHONE_ENABLED", "0".into());
    env.insert("TWILIO_ACCOUNT_SID", "invalid".into());
    assert!(Settings::from_lookup(|k| env.get(k).cloned())
        .unwrap()
        .is_none());
    for missing in values()
        .keys()
        .filter(|k| **k != "AUGMENTAGENT_PHONE_ENABLED" && **k != "AUGMENTAGENT_PHONE_BIND")
    {
        let mut env = values();
        env.remove(missing);
        assert!(
            Settings::from_lookup(|k| env.get(k).cloned()).is_err(),
            "missing {missing}"
        );
    }
    for (key, bad) in [
        ("AUGMENTAGENT_PHONE_ENABLED", "yes"),
        ("AUGMENTAGENT_PHONE_BIND", "0.0.0.0:8878"),
        ("AUGMENTAGENT_PHONE_PUBLIC_URL", "http://phone.example.test"),
        ("AUGMENTAGENT_PHONE_OWNER_LINKS", "[]"),
        (
            "AUGMENTAGENT_PHONE_OWNER_LINKS",
            r#"[{"platform":"slack","account":"T","surface_owner":"U","owner":"O","extra":"unexpected"}]"#,
        ),
        ("TWILIO_ACCOUNT_SID", "bad"),
    ] {
        let mut env = values();
        env.insert(key, bad.into());
        assert!(
            Settings::from_lookup(|k| env.get(k).cloned()).is_err(),
            "invalid {key}"
        );
    }
    let env = values();
    let settings = Settings::from_lookup(|k| env.get(k).cloned())
        .unwrap()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store =
        std::sync::Arc::new(augmentagent_store::Store::open(dir.path().join("db")).unwrap());
    let configured = settings.build(store.clone()).unwrap();
    assert_eq!(configured.bind.to_string(), "127.0.0.1:8878");
    // Explicit links don't invent authentication bindings or import a number.
    let account = augmentagent_store::SurfaceAccountRef::new(
        augmentagent_store::SurfacePlatform::new("slack").unwrap(),
        "T-fixture",
    )
    .unwrap();
    let owner = augmentagent_store::SurfaceOwnerRef::new(account.clone(), "U-fixture").unwrap();
    let chat = augmentagent_store::SurfaceConversationRef::new(account, "D-owner", None).unwrap();
    assert!(configured.service.authorize(&owner, &chat).is_err());
    assert!(store.phone_active_calls().unwrap().is_empty());
}

#[test]
fn debug_events_flag_is_explicit_and_fails_closed_on_junk() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        std::sync::Arc::new(augmentagent_store::Store::open(dir.path().join("db")).unwrap());
    for (value, expected) in [("1", true), ("0", false)] {
        let mut env = values();
        env.insert("AUGMENTAGENT_PHONE_DEBUG_EVENTS", value.into());
        let configured = Settings::from_lookup(|k| env.get(k).cloned())
            .unwrap()
            .unwrap()
            .build(store.clone())
            .unwrap();
        assert_eq!(configured.service.debug_events(), expected, "{value}");
    }
    let mut env = values();
    env.insert("AUGMENTAGENT_PHONE_DEBUG_EVENTS", "yes".into());
    assert!(Settings::from_lookup(|k| env.get(k).cloned()).is_err());
    // Absent: on for non-production (debug) builds only.
    let env = values();
    let configured = Settings::from_lookup(|k| env.get(k).cloned())
        .unwrap()
        .unwrap()
        .build(store)
        .unwrap();
    assert_eq!(configured.service.debug_events(), cfg!(debug_assertions));
}
