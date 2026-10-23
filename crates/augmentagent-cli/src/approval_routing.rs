//! #1289 — where `serve` posts approval cards.
//!
//! `AUGMENTAGENT_APPROVAL_SURFACES` lists the surfaces that get cards:
//! `discord`, `slack` or both (`discord,slack`). Unset (or `auto`) means
//! every surface that is configured: Discord when `DISCORD_BOT_TOKEN` is
//! set, Slack when the interactive Slack app is installed with a bound
//! owner. A listed surface that is not configured simply gets nothing; an
//! unknown word is a configuration error `serve` reports and then falls
//! back to `auto`.
//!
//! `AUGMENTAGENT_SLACK_APPROVAL_CHANNEL` picks where Slack cards go: `dm`
//! (default, the owner's DM with the app) or `control` (the bound private
//! control channel, `augmentagent slack app owner control set`).

pub const SURFACES_ENV: &str = "AUGMENTAGENT_APPROVAL_SURFACES";
pub const SLACK_CHANNEL_ENV: &str = "AUGMENTAGENT_SLACK_APPROVAL_CHANNEL";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routing {
    pub discord: bool,
    pub slack: bool,
}

impl Routing {
    pub const AUTO: Routing = Routing {
        discord: true,
        slack: true,
    };

    /// `Err` carries the rejected word.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::AUTO);
        };
        if v.eq_ignore_ascii_case("auto") {
            return Ok(Self::AUTO);
        }
        let mut routing = Routing {
            discord: false,
            slack: false,
        };
        for word in v.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            match word.to_ascii_lowercase().as_str() {
                "discord" => routing.discord = true,
                "slack" => routing.slack = true,
                _ => return Err(word.to_string()),
            }
        }
        Ok(routing)
    }

    pub fn from_env() -> Result<Self, String> {
        Self::parse(std::env::var(SURFACES_ENV).ok().as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackChannel {
    Dm,
    Control,
}

impl SlackChannel {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            None => Ok(Self::Dm),
            Some(v) if v.eq_ignore_ascii_case("dm") => Ok(Self::Dm),
            Some(v) if v.eq_ignore_ascii_case("control") => Ok(Self::Control),
            Some(v) => Err(v.to_string()),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        Self::parse(std::env::var(SLACK_CHANNEL_ENV).ok().as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_auto_routes_to_every_configured_surface() {
        assert_eq!(Routing::parse(None), Ok(Routing::AUTO));
        assert_eq!(Routing::parse(Some(" ")), Ok(Routing::AUTO));
        assert_eq!(Routing::parse(Some("AUTO")), Ok(Routing::AUTO));
    }

    #[test]
    fn a_list_routes_to_exactly_those_surfaces() {
        assert_eq!(
            Routing::parse(Some("slack")),
            Ok(Routing {
                discord: false,
                slack: true
            })
        );
        assert_eq!(
            Routing::parse(Some("Discord")),
            Ok(Routing {
                discord: true,
                slack: false
            })
        );
        assert_eq!(Routing::parse(Some("discord, slack")), Ok(Routing::AUTO));
        assert_eq!(
            Routing::parse(Some("slack,teams")),
            Err("teams".to_string())
        );
    }

    #[test]
    fn slack_cards_go_to_the_dm_unless_the_control_channel_is_chosen() {
        assert_eq!(SlackChannel::parse(None), Ok(SlackChannel::Dm));
        assert_eq!(SlackChannel::parse(Some("dm")), Ok(SlackChannel::Dm));
        assert_eq!(
            SlackChannel::parse(Some("Control")),
            Ok(SlackChannel::Control)
        );
        assert_eq!(
            SlackChannel::parse(Some("general")),
            Err("general".to_string())
        );
    }
}
