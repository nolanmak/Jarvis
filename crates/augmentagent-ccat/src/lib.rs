//! CCat is a bounded decision gate for externally visible Jarvis actions.
//!
//! It deliberately returns one of four machine-readable outcomes. It is not
//! an authorization system and must be composed with deterministic checks and
//! the existing approval flow.

pub mod policy;
pub mod receipt;
pub mod redact;
pub mod seacat;

pub use policy::{
    evaluate, Answer, CcatError, Decision, DecisionOutcome, DecisionProvider, Policy, Question,
    QuestionKind, EXTERNAL_MESSAGE_SEND_POLICY, PUBLIC_GIT_PUSH_POLICY,
};
pub use receipt::{now_epoch, ApprovalReceipt, ReceiptSigner};
pub use redact::{redact, RedactedPayload};
pub use seacat::SeaCatDecisionProvider;
