//! Explicit owner probe scope, separate from ordinary fallback behavior.
use std::future::Future;

tokio::task_local! { static STRICT: bool; }
pub fn can_rescue_native() -> bool {
    !STRICT.try_with(|value| *value).unwrap_or(false)
}
pub async fn scope<F: Future>(future: F) -> F::Output {
    STRICT.scope(true, future).await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn atomic_gateway_off_keeps_inflight_snapshot_and_next_call_direct() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model-router.json");
        let config = crate::model_router::tests::fixture();
        std::fs::write(&path, config.to_string()).unwrap();
        let snapshot = crate::model_router::load_from(&path).unwrap().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, continue_call) = tokio::sync::oneshot::channel();
        let call = tokio::spawn(async move {
            crate::model_router::SNAPSHOT
                .scope(Some(snapshot), async {
                    started.send(()).unwrap();
                    continue_call.await.unwrap();
                    assert_eq!(
                        crate::model_router::current().unwrap().unwrap().mode,
                        "auto"
                    );
                })
                .await;
        });
        ready.await.unwrap();
        let mut direct = config;
        direct["mode"] = serde_json::json!("direct");
        let pending = directory.path().join("pending");
        std::fs::write(&pending, direct.to_string()).unwrap();
        std::fs::rename(pending, &path).unwrap();
        assert_eq!(
            crate::model_router::load_from(&path).unwrap().unwrap().mode,
            "direct"
        );
        release.send(()).unwrap();
        call.await.unwrap();
    }

    #[tokio::test]
    async fn routed_probe_cannot_be_rescued_by_a_native_login() {
        assert!(super::can_rescue_native());
        super::scope(async {
            assert!(
                !super::can_rescue_native(),
                "routed probe would falsely pass through direct rescue"
            );
        })
        .await;
        assert!(
            super::can_rescue_native(),
            "ordinary calls keep native rescue"
        );
    }
}
