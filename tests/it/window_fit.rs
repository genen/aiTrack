// SPDX-License-Identifier: GPL-3.0-or-later
//! The window auto-fit decision (pure functions, no window, no clock).

use aitrack::view::{
    fit_decide, fit_height, FitAction, FitState, DEFAULT_INNER_MACOS_PT, DEFAULT_INNER_PT,
    FIT_MAX_RETRIES, FIT_SETTLE_MS, MIN_WINDOW_H,
};

#[test]
fn fit_removes_constant_extra_space_and_matches_windows_layout() {
    // Windows-like: window = content (200) + margin (8) + panels (24): no slack, same height.
    assert_eq!(fit_height(232.0, 208.0, 200.0, 8.0, 120.0, 700.0), 232.0);
    // macOS-like: 50 pt of empty strip above the panels is removed, whatever its cause.
    assert_eq!(fit_height(282.0, 258.0, 200.0, 8.0, 120.0, 700.0), 232.0);
    // title-bar inset counted twice shifts both window and panels: still the same answer
    assert_eq!(fit_height(310.0, 286.0, 228.0, 8.0, 120.0, 700.0), 260.0);
}

#[test]
fn fit_grows_when_content_is_clipped_and_respects_the_cap() {
    // content ends 40 pt below where the panels start: grow by 40
    assert_eq!(fit_height(300.0, 276.0, 316.0, 8.0, 120.0, 700.0), 348.0);
    assert_eq!(fit_height(300.0, 276.0, 900.0, 8.0, 120.0, 500.0), 500.0);
    assert_eq!(
        fit_height(300.0, 276.0, 0.0, 8.0, 120.0, 500.0),
        MIN_WINDOW_H.max(120.0)
    );
}

fn sent(h: f32, baseline: f32, age: u128) -> FitState {
    FitState {
        sent: Some(h),
        baseline,
        age_ms: Some(age),
        ..Default::default()
    }
}

#[test]
fn first_frame_requests_then_holds() {
    let mut st = FitState::default();
    let a = fit_decide(&st, 232.0, 240.0);
    assert_eq!(a, FitAction::Request(232.0));
    st.apply(a, 240.0);
    st.age_ms = Some(10);
    assert_eq!(fit_decide(&st, 232.0, 240.0), FitAction::Keep);
    // content changed: ask again
    assert_eq!(fit_decide(&st, 300.0, 240.0), FitAction::Request(300.0));
}

#[test]
fn hand_resize_stops_the_fit() {
    let mut st = sent(232.0, 240.0, FIT_SETTLE_MS + 1);
    // settled at the requested height: fine
    assert_eq!(fit_decide(&st, 232.0, 232.0), FitAction::Keep);
    // the user dragged it to 400
    let a = fit_decide(&st, 232.0, 400.0);
    assert_eq!(a, FitAction::UserSized);
    st.apply(a, 400.0);
    assert_eq!(fit_decide(&st, 300.0, 400.0), FitAction::Keep);
}

#[test]
fn ignored_request_is_retried_not_counted_as_hand_resize() {
    let mut st = sent(232.0, 240.0, FIT_SETTLE_MS + 1);
    for n in 0..FIT_MAX_RETRIES {
        let a = fit_decide(&st, 232.0, 240.0);
        assert_eq!(a, FitAction::Request(232.0));
        st.apply(a, 240.0);
        assert_eq!(st.retries, n + 1);
        assert!(!st.user_sized);
        st.age_ms = Some(FIT_SETTLE_MS + 1);
    }
    let a = fit_decide(&st, 232.0, 240.0);
    assert_eq!(a, FitAction::GiveUp);
    st.apply(a, 240.0);
    assert!(st.gave_up && !st.user_sized);
    assert_eq!(fit_decide(&st, 300.0, 240.0), FitAction::Keep);
}

#[test]
fn mac_default_is_larger_and_the_minimum_is_unchanged() {
    assert!(DEFAULT_INNER_MACOS_PT.0 > DEFAULT_INNER_PT.0);
    assert!(DEFAULT_INNER_MACOS_PT.1 > DEFAULT_INNER_PT.1);
    assert_eq!(DEFAULT_INNER_PT, (310.0, 240.0));
    assert_eq!(MIN_WINDOW_H, 120.0);
    // short content on a taller window: the fit shrinks it down to the content (user may also drag)
    let h = fit_height(500.0, 476.0, 100.0, 8.0, MIN_WINDOW_H, 800.0);
    assert_eq!(h, 132.0);
}
