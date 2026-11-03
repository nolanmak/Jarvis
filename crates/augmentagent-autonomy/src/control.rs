//! C8 (D10) — what the notice buttons do: Undo (remove the guest the plan
//! added, unless they accepted), Pause this person, Pause all, and Confirm
//! a pending trusted contact. Every control is idempotent.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::autonomy::AutonomyControl;
use augmentagent_store::Store;

use crate::calendar::CalendarOps;

pub struct Controls {
    pub store: Arc<Store>,
    pub calendar: Option<Arc<dyn CalendarOps>>,
    pub config_path: PathBuf,
}

impl Controls {
    fn now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

#[async_trait]
impl AutonomyControl for Controls {
    async fn undo(&self, plan_id: i64) -> String {
        let _ = (plan_id, Self::now(), &self.store, &self.calendar);
        todo!("C8: undo")
    }
    async fn pause(&self, slug: &str) -> String {
        let _ = slug;
        todo!("C8: pause")
    }
    async fn pause_all(&self) -> String {
        todo!("C8: pause all")
    }
    async fn confirm(&self, slug: &str, entry_hash_prefix: &str) -> String {
        let _ = (slug, entry_hash_prefix, &self.config_path);
        todo!("C8: confirm")
    }
}
