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
