use pretty_assertions::assert_eq;

use super::*;

#[test]
fn the_first_frame_is_not_held() {
    let now = Instant::now();
    assert_eq!(FrameLimiter::default().deadline(now), now);
}

#[test]
fn a_frame_waits_a_thirtieth_of_a_second_after_the_last() {
    let t0 = Instant::now();
    let mut limiter = FrameLimiter::default();
    limiter.drawn(t0);
    assert_eq!(
        limiter.deadline(t0 + Duration::from_millis(1)),
        t0 + FRAME_INTERVAL
    );
    let late = t0 + Duration::from_millis(50);
    assert_eq!(limiter.deadline(late), late);
}
