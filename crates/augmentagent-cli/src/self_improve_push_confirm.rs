//! #2547 — confirm a dispatched push by observing its exact head.
//! Production shape (2026-11-24 21:10Z): workflow #2424 pushed `e1306074` to
//! PR #2545. A single read of GitHub's ref, one second later, still returned
//! the old head. The approved review was discarded and the lease outlived the
//! turn. The idle observer saw the landed head nine minutes later.
use super::*;

pub(super) const PUSH_OBSERVATION_ATTEMPTS: u32 = 5;
pub(super) const PUSH_OBSERVATION_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Only the exact expected head confirms the push; any other answer, after
/// the bounded re-reads, leaves the pending intent for the idle observer.
pub(super) async fn confirm_pushed_head<F, Fut>(expected: &str, pushed: bool, delay: std::time::Duration, mut observe: F) -> Result<String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<String>>>,
{
    let mut failure = None;
    for attempt in 0..PUSH_OBSERVATION_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(delay).await;
        }
        match observe().await {
            Ok(Some(head)) if head == expected => return Ok(head),
            Ok(_) => failure = None,
            Err(error) => failure = Some(error),
        }
    }
    match failure {
        Some(error) => Err(error.context(format!("push outcome not verified; preserved pending intent (command success: {pushed})"))),
        None => anyhow::bail!("push outcome not verified; preserved pending intent (command success: {pushed})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const OLD: &str = "1176fe392f0a9dc7610047cad72dc3ae4a442464";
    const NEW: &str = "e1306074494739c0ac1416abc022031d95448508";

    fn reader<'a>(answers: &'a [Result<Option<&'static str>, &'static str>], calls: &'a Cell<usize>)
        -> impl FnMut() -> std::future::Ready<Result<Option<String>>> + 'a {
        move || {
            let call = calls.get();
            calls.set(call + 1);
            let answer = answers[call.min(answers.len() - 1)];
            std::future::ready(answer.map(|head| head.map(str::to_owned)).map_err(anyhow::Error::msg))
        }
    }

    /// #2545's exact read sequence: the ref API still served the old head
    /// right after the push landed, then the new head.
    #[tokio::test]
    async fn a_landed_push_is_confirmed_after_a_stale_ref_read() {
        let calls = Cell::new(0);
        let answers = [Ok(Some(OLD)), Ok(Some(NEW))];
        let head = confirm_pushed_head(NEW, true, std::time::Duration::ZERO, reader(&answers, &calls)).await;
        assert_eq!(head.ok().as_deref(), Some(NEW), "a stale read discarded an approved, landed push");
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn a_transient_observation_failure_is_re_read() {
        let calls = Cell::new(0);
        let answers = [Err("HTTP 502"), Ok(Some(NEW))];
        let head = confirm_pushed_head(NEW, true, std::time::Duration::ZERO, reader(&answers, &calls)).await;
        assert_eq!(head.ok().as_deref(), Some(NEW));
    }

    #[tokio::test]
    async fn another_head_is_never_accepted_and_reads_are_bounded() {
        for answers in [vec![Ok(Some(OLD))], vec![Ok(None)], vec![Err("HTTP 502")]] {
            let calls = Cell::new(0);
            let error = confirm_pushed_head(NEW, false, std::time::Duration::ZERO, reader(&answers, &calls)).await
                .expect_err("only the exact bound head confirms a push");
            assert!(format!("{error:#}").contains("push outcome not verified; preserved pending intent (command success: false)"), "{error:#}");
            assert_eq!(calls.get(), PUSH_OBSERVATION_ATTEMPTS as usize, "bounded re-reads");
        }
        assert!(PUSH_OBSERVATION_ATTEMPTS > 1 && PUSH_OBSERVATION_ATTEMPTS <= 6);
        assert!(PUSH_OBSERVATION_DELAY * PUSH_OBSERVATION_ATTEMPTS <= std::time::Duration::from_secs(15),
            "the re-read window stays well inside the lease");
    }
}
