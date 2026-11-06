//! Original owner-preparation association, never native/input authority.
use super::{PreparedLaunchIntent,RuntimeOwner};
use anyhow::{ensure,Result};
use serde::{Deserialize,Serialize};

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerPreparationContext {
    version:u32,owner:RuntimeOwner,request_id:String,input_context_sha256:String,
}
tokio::task_local! {static CURRENT:OwnerPreparationContext;}
impl OwnerPreparationContext {
    pub fn new(owner:RuntimeOwner,request_id:String,input_context_sha256:String)->Result<Self> {
        let context=Self {version:1,owner,request_id,input_context_sha256};context.validate()?;Ok(context)
    }
    fn validate(&self)->Result<()> {
        ensure!(self.version==1 && self.owner.lease_generation>0 && !self.owner.workflow_id.is_empty()
            && self.owner.workflow_id.len()<=1024 && !self.request_id.is_empty() && self.request_id.len()<=1024
            && self.input_context_sha256.len()==64 && self.input_context_sha256.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')),
            "invalid original owner preparation context");Ok(())
    }
    pub(crate) fn validate_launch(&self,intent:&PreparedLaunchIntent)->Result<()> {
        self.validate_owner(&intent.source_owner)
    }
    pub(crate) fn validate_owner(&self,owner:&RuntimeOwner)->Result<()> {
        self.validate()?;ensure!(&self.owner==owner,"owner preparation launch belongs to another original owner");Ok(())
    }
    /// Associates an independent preparation operation with its original input.
    /// The transport must separately enforce fresh, tool-free review. Neither
    /// this scope nor deserialized context can authorize provider/body input.
    pub async fn scope<T>(&self,work:impl std::future::Future<Output=T>)->T {
        CURRENT.scope(self.clone(),work).await
    }
}
pub(super) fn current()->Option<OwnerPreparationContext> {CURRENT.try_with(Clone::clone).ok()}
