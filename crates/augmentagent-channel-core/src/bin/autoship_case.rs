use std::sync::atomic::{AtomicI64, Ordering};

static NOW: AtomicI64 = AtomicI64::new(1_793_509_200);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_gettime(_clock: libc::clockid_t, tp: *mut libc::timespec) -> libc::c_int {
    unsafe {
        (*tp).tv_sec = NOW.load(Ordering::SeqCst) as libc::time_t;
        (*tp).tv_nsec = 0;
    }
    0
}

fn at(epoch: i64, message: &str) -> Option<i64> {
    NOW.store(epoch, Ordering::SeqCst);
    augmentagent_channel_core::reasoner::parse_reset_hint(message).map(|time| time.timestamp())
}

fn main() {
    unsafe { std::env::set_var("TZ", "America/New_York") };
    let values = vec![
        at(1_793_509_200, "try again at Nov 1st, 2026 1:32 AM"),
        at(1_793_509_200, "try again at 1:32 AM"),
        at(1_793_511_900, "try again at 1:32 AM"),
        at(1_793_511_900, "resets 1:32 AM (America/New_York)"),
        at(1_772_949_600, "try again at 2:30 AM"),
        at(1_793_509_200, "try again at Nov 1st, 2026 12:30 AM"),
        at(1_793_515_800, "try again at 1:30 AM"),
        at(1_793_509_200, "resets 12:00 PM (America/New_York)"),
        at(1_793_509_200, "try again at Nov 10th, 2026 1:00 AM"),
        at(1_793_509_200, "try again at whenever"),
    ];
    println!("{}", serde_json::to_string(&values).unwrap());
}
