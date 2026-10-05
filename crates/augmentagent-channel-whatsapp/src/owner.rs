//! Owner admission for the interactive surface. A chat match alone grants no authority.
use crate::types::WaMessage;
use augmentagent_store::WhatsappOwnerConfig;

pub fn admits(owner: &WhatsappOwnerConfig, message: &WaMessage) -> bool {
    let account = crate::types::Jid::new(&message.metadata.account_jid);
    if !account.is_personal()
        || account.user() != owner.phone
        || !message.chat.is_personal()
        || !message.sender.is_personal()
        || message.chat.bare() != owner.control_chat_jid
        || message.sender.bare() != owner.owner_jid
        || message.metadata.agent_generated
        || message.metadata.is_edit
        || message.metadata.is_revoke
        || message.metadata.is_view_once
        || message.metadata.is_ephemeral
    {
        return false;
    }
    match owner.mode.as_str() {
        "self_chat" => {
            owner.owner_jid == format!("{}@s.whatsapp.net", owner.phone)
                && message.from_me
                && message.metadata.origin_verified
        }
        "dedicated" => {
            owner.owner_jid != format!("{}@s.whatsapp.net", owner.phone) && !message.from_me
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner(mode: &str) -> WhatsappOwnerConfig {
        WhatsappOwnerConfig {
            phone: "15550000000".into(),
            owner_jid: "15550000000@s.whatsapp.net".into(),
            control_chat_jid: "15550000000@s.whatsapp.net".into(),
            mode: mode.into(),
        }
    }
    fn message() -> WaMessage {
        serde_json::from_value(
            serde_json::json!({"id":"human", "chat":"15550000000@s.whatsapp.net",
            "sender":"15550000000:7@s.whatsapp.net", "text":"hello", "timestamp":1,
            "from_me":true,"origin_verified":true,"account_jid":"15550000000:3@s.whatsapp.net"}),
        )
        .unwrap()
    }
    #[test]
    fn self_chat_accepts_owner_other_device_but_never_agent_echo_or_unknown_origin() {
        let owner = owner("self_chat");
        let mut m = message();
        assert!(admits(&owner, &m));
        m.metadata.agent_generated = true;
        assert!(!admits(&owner, &m));
        m.metadata.agent_generated = false;
        m.metadata.origin_verified = false;
        assert!(!admits(&owner, &m));
        m.metadata.origin_verified = true;
        m.from_me = false;
        assert!(!admits(&owner, &m));
    }
    #[test]
    fn dedicated_requires_both_owner_sender_and_control_chat() {
        let mut owner = owner("dedicated");
        owner.phone = "15551111111".into();
        let mut m = message();
        m.metadata.account_jid = "15551111111:3@s.whatsapp.net".into();
        m.from_me = false;
        assert!(admits(&owner, &m));
        m.sender.0 = "15552222222@s.whatsapp.net".into();
        assert!(!admits(&owner, &m));
        m = message();
        m.from_me = false;
        m.chat.0 = "15552222222@s.whatsapp.net".into();
        assert!(!admits(&owner, &m));
        m = message();
        assert!(!admits(&owner, &m));
    }
    #[test]
    fn group_edit_revoke_and_disappearing_messages_are_not_owner_commands() {
        let owner = owner("self_chat");
        for key in ["is_edit", "is_revoke", "is_view_once", "is_ephemeral"] {
            let mut value = serde_json::to_value(message()).unwrap();
            value[key] = true.into();
            assert!(
                !admits(&owner, &serde_json::from_value(value).unwrap()),
                "{key}"
            );
        }
        let mut m = message();
        m.chat.0 = "123@g.us".into();
        assert!(!admits(&owner, &m));
    }
}
