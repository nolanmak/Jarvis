//! Cross-platform compose-once adapter. One source draft fans out into N
//! per-platform variants (Instagram caption, LinkedIn post, X single-tweet
//! or thread). Each variant is approval-gated independently. See issue #53.

pub mod media;
pub mod prompts;
pub mod types;
