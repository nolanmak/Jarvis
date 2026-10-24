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
fn ac03_voice_providers_default_to_deepgram_and_elevenlabs_and_fail_closed_on_bad_values() {
    use augmentagent_phone::voice::{SttProvider, TtsProvider, VoiceConfig, VoiceConfigError};
    use augmentagent_phone::Error;
    let env = values();
    let settings = Settings::from_lookup(|k| env.get(k).cloned())
        .unwrap()
        .unwrap();
    let voice = settings.voice();
    assert_eq!(voice.stt_provider(), SttProvider::Deepgram);
    assert_eq!(voice.stt_model(), "nova-3-general");
    assert_eq!(voice.tts_provider(), TtsProvider::ElevenLabs);
    assert_eq!(voice.tts_voice(), "UgBBYS2sOqTuMpoF3BR0-flash_v2_5");
    assert_eq!(*voice, VoiceConfig::default());

    let mut env = values();
    env.insert("AUGMENTAGENT_PHONE_STT_PROVIDER", "Google".into());
    env.insert("AUGMENTAGENT_PHONE_STT_MODEL", "telephony".into());
    env.insert("AUGMENTAGENT_PHONE_TTS_PROVIDER", "Amazon".into());
    env.insert("AUGMENTAGENT_PHONE_TTS_VOICE", "Joanna".into());
    let voice = Settings::from_lookup(|k| env.get(k).cloned())
        .unwrap()
        .unwrap()
        .voice()
        .clone();
    assert_eq!(voice.stt_provider(), SttProvider::Google);
    assert_eq!(voice.stt_model(), "telephony");
    assert_eq!(voice.tts_provider(), TtsProvider::Amazon);
    assert_eq!(voice.tts_voice(), "Joanna");

    // Provider names are an exact, case-sensitive allow list. Voice and model
    // strings are ^[A-Za-z0-9._-]{1,96}$: a quote or angle bracket could
    // otherwise break out of the TwiML attribute.
    let long = "a".repeat(97);
    for (key, bad, expected) in [
        (
            "AUGMENTAGENT_PHONE_STT_PROVIDER",
            "Whisper",
            VoiceConfigError::UnknownSttProvider,
        ),
        (
            "AUGMENTAGENT_PHONE_STT_PROVIDER",
            "deepgram",
            VoiceConfigError::UnknownSttProvider,
        ),
        (
            "AUGMENTAGENT_PHONE_TTS_PROVIDER",
            "OpenAI",
            VoiceConfigError::UnknownTtsProvider,
        ),
        (
            "AUGMENTAGENT_PHONE_TTS_PROVIDER",
            "elevenlabs",
            VoiceConfigError::UnknownTtsProvider,
        ),
        (
            "AUGMENTAGENT_PHONE_TTS_VOICE",
            "Joanna\" welcomeGreeting=\"pwned",
            VoiceConfigError::InvalidVoice,
        ),
        (
            "AUGMENTAGENT_PHONE_TTS_VOICE",
            "<Joanna>",
            VoiceConfigError::InvalidVoice,
        ),
        (
            "AUGMENTAGENT_PHONE_TTS_VOICE",
            "Joanna Neural",
            VoiceConfigError::InvalidVoice,
        ),
        (
            "AUGMENTAGENT_PHONE_TTS_VOICE",
            long.as_str(),
            VoiceConfigError::InvalidVoice,
        ),
        (
            "AUGMENTAGENT_PHONE_STT_MODEL",
            "nova 3",
            VoiceConfigError::InvalidSttModel,
        ),
        (
            "AUGMENTAGENT_PHONE_STT_MODEL",
            "nova-3&x=1",
            VoiceConfigError::InvalidSttModel,
        ),
    ] {
        let mut env = values();
        env.insert(key, bad.into());
        match Settings::from_lookup(|k| env.get(k).cloned()) {
            Err(Error::Voice(error)) => assert_eq!(error, expected, "{key}={bad:?}"),
            Err(other) => panic!("{key}={bad:?}: unexpected error {other:?}"),
            Ok(_) => panic!("{key}={bad:?}: accepted"),
        }
    }
    // An empty value behaves like an unset variable, as in env files.
    let mut env = values();
    env.insert("AUGMENTAGENT_PHONE_TTS_VOICE", "".into());
    env.insert("AUGMENTAGENT_PHONE_STT_PROVIDER", " ".into());
    assert_eq!(
        *Settings::from_lookup(|k| env.get(k).cloned())
            .unwrap()
            .unwrap()
            .voice(),
        VoiceConfig::default()
    );
    // Google TTS has no vetted default voice here: it must be explicit.
    let mut env = values();
    env.insert("AUGMENTAGENT_PHONE_TTS_PROVIDER", "Google".into());
    assert!(matches!(
        Settings::from_lookup(|k| env.get(k).cloned()),
        Err(Error::Voice(VoiceConfigError::VoiceRequired))
    ));
    env.insert(
        "AUGMENTAGENT_PHONE_TTS_VOICE",
        "en-US-Chirp3-HD-Aoede".into(),
    );
    let voice = Settings::from_lookup(|k| env.get(k).cloned())
        .unwrap()
        .unwrap()
        .voice()
        .clone();
    assert_eq!(voice.tts_provider(), TtsProvider::Google);
    assert_eq!(voice.tts_voice(), "en-US-Chirp3-HD-Aoede");
    // Switching only the TTS provider to Amazon keeps the proven Joanna default.
    let mut env = values();
    env.insert("AUGMENTAGENT_PHONE_TTS_PROVIDER", "Amazon".into());
    assert_eq!(
        Settings::from_lookup(|k| env.get(k).cloned())
            .unwrap()
            .unwrap()
            .voice()
            .tts_voice(),
        "Joanna"
    );
    let mut env = values();
    env.insert("AUGMENTAGENT_PHONE_STT_PROVIDER", "Google".into());
    assert_eq!(
        Settings::from_lookup(|k| env.get(k).cloned())
            .unwrap()
            .unwrap()
            .voice()
            .stt_model(),
        "telephony"
    );
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

fn load(
    env: &HashMap<&'static str, String>,
) -> Result<Option<Settings>, augmentagent_phone::Error> {
    Settings::from_lookup(|k| env.get(k).cloned())
}

// TDD step 8 / AC4 / AC8: barge-in settings are config-loaded with documented
// defaults; a false-interruption timeout outside 500–5,000 ms fails with a
// typed error naming the variable; other junk fails closed.
#[test]
fn barge_in_settings_load_with_defaults_and_bounded_timeout() {
    use augmentagent_phone::{relay::DEFAULT_BACKCHANNELS, Error};
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store =
        std::sync::Arc::new(augmentagent_store::Store::open(dir.path().join("db")).unwrap());
    let build = |env: &HashMap<&'static str, String>| {
        load(env).unwrap().unwrap().build(store.clone()).unwrap()
    };
    let defaults = build(&values());
    let barge = defaults.service.barge_in();
    assert_eq!(barge.min_words(), 3);
    assert_eq!(barge.backchannels(), DEFAULT_BACKCHANNELS);
    assert_eq!(barge.false_interrupt(), Duration::from_millis(2_000));
    assert!(barge.resume());

    let mut env = values();
    env.insert("AUGMENTAGENT_PHONE_BARGE_MIN_WORDS", "1".into());
    env.insert(
        "AUGMENTAGENT_PHONE_BACKCHANNEL_PHRASES",
        "OK, Mm-hmm ,sure thing".into(),
    );
    env.insert("AUGMENTAGENT_PHONE_FALSE_INTERRUPT_MS", "800".into());
    env.insert("AUGMENTAGENT_PHONE_RESUME_FALSE_INTERRUPT", "0".into());
    let configured = build(&env);
    let barge = configured.service.barge_in();
    assert_eq!(barge.min_words(), 1);
    assert_eq!(barge.backchannels(), ["ok", "mm hmm", "sure thing"]);
    assert_eq!(barge.false_interrupt(), Duration::from_millis(800));
    assert!(!barge.resume());

    for edge in ["500", "5000"] {
        let mut env = values();
        env.insert("AUGMENTAGENT_PHONE_FALSE_INTERRUPT_MS", edge.into());
        assert!(load(&env).is_ok(), "{edge}");
    }
    for bad in ["499", "5001", "0", "-1", "2s", ""] {
        let mut env = values();
        env.insert("AUGMENTAGENT_PHONE_FALSE_INTERRUPT_MS", bad.into());
        match load(&env) {
            Err(Error::InvalidSetting(key)) => {
                assert_eq!(key, "AUGMENTAGENT_PHONE_FALSE_INTERRUPT_MS", "{bad}")
            }
            Err(other) => panic!("{bad}: untyped {other:?}"),
            Ok(_) => panic!("{bad}: accepted"),
        }
    }
    for (key, bad) in [
        ("AUGMENTAGENT_PHONE_BARGE_MIN_WORDS", "0"),
        ("AUGMENTAGENT_PHONE_BARGE_MIN_WORDS", "11"),
        ("AUGMENTAGENT_PHONE_BARGE_MIN_WORDS", "three"),
        ("AUGMENTAGENT_PHONE_BACKCHANNEL_PHRASES", " , "),
        ("AUGMENTAGENT_PHONE_RESUME_FALSE_INTERRUPT", "yes"),
    ] {
        let mut env = values();
        env.insert(key, bad.into());
        assert!(
            matches!(load(&env), Err(Error::InvalidSetting(k)) if k == key),
            "{key}={bad}"
        );
    }
}

// #1546: the fast layer is off unless explicitly enabled; its settings are
// validated at load and fail closed.
#[test]
fn fast_layer_flags_default_off_and_fail_closed_on_junk() {
    use augmentagent_phone::fast::{FastLayerConfig, FastProvider};
    let dir = tempfile::tempdir().unwrap();
    let store =
        std::sync::Arc::new(augmentagent_store::Store::open(dir.path().join("db")).unwrap());
    let load = |pairs: &[(&'static str, &str)]| {
        let mut env = values();
        for (k, v) in pairs {
            env.insert(k, (*v).into());
        }
        Settings::from_lookup(|k| env.get(k).cloned())
    };
    let fast = |pairs: &[(&'static str, &str)]| {
        load(pairs)
            .unwrap()
            .unwrap()
            .build(store.clone())
            .unwrap()
            .service
            .fast_layer()
            .cloned()
    };
    assert_eq!(fast(&[]), None);
    assert_eq!(fast(&[("AUGMENTAGENT_PHONE_FAST_LAYER", "0")]), None);
    // Settings for a disabled layer are ignored, not half-applied.
    assert_eq!(
        fast(&[("AUGMENTAGENT_PHONE_FAST_PROVIDER", "cerebras")]),
        None
    );
    assert_eq!(
        fast(&[("AUGMENTAGENT_PHONE_FAST_LAYER", "1")]),
        Some(FastLayerConfig::default())
    );
    assert_eq!(
        fast(&[
            ("AUGMENTAGENT_PHONE_FAST_LAYER", "1"),
            ("AUGMENTAGENT_PHONE_FAST_PROVIDER", "cerebras"),
            ("AUGMENTAGENT_PHONE_MAX_TOOL_STEPS", "5"),
        ]),
        Some(FastLayerConfig {
            provider: FastProvider::Cerebras,
            max_tool_steps: 5,
            thinking_sound_url: None,
        })
    );
    for bad in [
        [("AUGMENTAGENT_PHONE_FAST_LAYER", "yes"), ("AUGMENTAGENT_PHONE_FAST_PROVIDER", "anthropic")],
        [("AUGMENTAGENT_PHONE_FAST_LAYER", "1"), ("AUGMENTAGENT_PHONE_FAST_PROVIDER", "openai")],
        [("AUGMENTAGENT_PHONE_FAST_LAYER", "1"), ("AUGMENTAGENT_PHONE_MAX_TOOL_STEPS", "0")],
        [("AUGMENTAGENT_PHONE_FAST_LAYER", "1"), ("AUGMENTAGENT_PHONE_MAX_TOOL_STEPS", "9")],
        [("AUGMENTAGENT_PHONE_FAST_LAYER", "1"), ("AUGMENTAGENT_PHONE_MAX_TOOL_STEPS", "three")],
    ] {
        assert!(load(&bad).is_err(), "{bad:?}");
    }
}

// AC11: the thinking sound must be https on the operator's own public origin.
#[test]
fn thinking_sound_url_must_be_on_the_operator_origin() {
    let load = |url: &str| {
        let mut env = values();
        env.insert("AUGMENTAGENT_PHONE_FAST_LAYER", "1".into());
        env.insert("AUGMENTAGENT_PHONE_THINKING_SOUND_URL", url.into());
        Settings::from_lookup(|k| env.get(k).cloned())
    };
    let dir = tempfile::tempdir().unwrap();
    let store =
        std::sync::Arc::new(augmentagent_store::Store::open(dir.path().join("db")).unwrap());
    let configured = load("https://phone.example.test/sounds/thinking.mp3")
        .unwrap()
        .unwrap()
        .build(store)
        .unwrap();
    assert_eq!(
        configured.service.fast_layer().unwrap().thinking_sound_url.as_deref(),
        Some("https://phone.example.test/sounds/thinking.mp3")
    );
    for bad in [
        "https://evil.example/sounds/thinking.mp3",
        "http://phone.example.test/sounds/thinking.mp3",
        "https://phone.example.test:8443/thinking.mp3",
        "https://user:pw@phone.example.test/thinking.mp3",
        "https://phone.example.test.evil.example/thinking.mp3",
        "not a url",
    ] {
        assert!(load(bad).is_err(), "{bad}");
    }
}

// #1553: quiet hours for agent-proposed calls are explicit and fail closed.
#[test]
fn proposal_quiet_hours_and_time_zone_fail_closed_on_junk() {
    for (key, value) in [
        ("AUGMENTAGENT_PHONE_QUIET_HOURS", "22:00-08:00"),
        ("AUGMENTAGENT_PHONE_QUIET_HOURS", "off"),
        ("AUGMENTAGENT_PHONE_QUIET_HOURS", "21:30-07:15"),
        ("AUGMENTAGENT_TIMEZONE", "America/New_York"),
    ] {
        let mut env = values();
        env.insert(key, value.into());
        assert!(Settings::from_lookup(|k| env.get(k).cloned()).is_ok(), "{key}={value}");
    }
    for (key, value) in [
        ("AUGMENTAGENT_PHONE_QUIET_HOURS", "nightly"),
        ("AUGMENTAGENT_PHONE_QUIET_HOURS", "22:00-22:00"),
        ("AUGMENTAGENT_PHONE_QUIET_HOURS", "24:00-08:00"),
        ("AUGMENTAGENT_TIMEZONE", "Mars/Base"),
    ] {
        let mut env = values();
        env.insert(key, value.into());
        assert!(Settings::from_lookup(|k| env.get(k).cloned()).is_err(), "{key}={value}");
    }
}
