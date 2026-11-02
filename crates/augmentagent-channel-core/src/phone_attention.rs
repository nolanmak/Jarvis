//! Narrow read-only data seam for routine phone attention checks. The source
//! adapter owns source selection, freshness and verified findings; native work
//! still interprets every other request and retains its approval boundary.
use async_trait::async_trait;
use augmentagent_store::phone::PhoneOrigin;

#[async_trait]
pub trait AttentionProvider: Send + Sync {
    /// None means this origin has no configured attention data adapter.
    async fn check(&self, origin: &PhoneOrigin) -> anyhow::Result<Option<String>>;
}
