//! Cross-platform compose-once adapter (#53). One source draft fans out into
//! N per-platform variants (Instagram caption, LinkedIn post, X single-tweet
//! or thread), each approval-gated independently. Pure text transform over
//! the shared `Reasoner` — no posting here; the channels own that.

pub mod adapter;
pub mod media;
pub mod preview;
pub mod prompts;
pub mod publish;
pub mod socialapi;
pub mod types;

pub use adapter::fan_out;
pub use preview::{preview_all, variant_card};
pub use socialapi::{family_card, fan_out_socialapi, SocialTarget, TargetVariant};
pub use publish::{
    fan_out_publish, FanOutReport, FanOutTargets, PostContent, PublishOpts, PublishOutcome,
    PublishTarget, SocialPublisher,
};
pub use types::{MediaSpec, Platform, PlatformVariant, SourceDraft};
