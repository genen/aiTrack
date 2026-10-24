// SPDX-License-Identifier: GPL-3.0-or-later
//! Pricing: lookup, live-list parsing, caching rules and cost maths. Never touches the network:
//! the download is an injected closure.

use crate::common;
use aitrack::pricing::*;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::TempDir;
use serde_json::json;
use std::cell::Cell;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// A community-list fixture with the shapes seen in the wild plus junk that must be skipped.
fn live_fixture() -> String {
    json!({
        "sample_spec": {"max_tokens": "LEGACY", "input_cost_per_token": 0.0, "litellm_provider": "x"},
        "gpt-4o": {"input_cost_per_token": 2.5e-6, "output_cost_per_token": 1e-5},
        // two ways to spell Sonnet 5.5; the bare key (3/15) wins over provider-prefixed ones
        "claude-sonnet-5-5": {
            "input_cost_per_token": 2e-6, "output_cost_per_token": 1e-5,
            "cache_read_input_token_cost": 2e-7, "cache_creation_input_token_cost": 2.5e-6,
            "cache_creation_input_token_cost_above_1hr": 4e-6,
        },
        "anthropic/claude-sonnet-5-5": {"input_cost_per_token": 9e-6, "output_cost_per_token": 9e-5},
        "bedrock/us-east-1/anthropic.claude-sonnet-5-5-v1:0": {"input_cost_per_token": 8e-6, "output_cost_per_token": 8e-5},
        // dated + prefixed ids map onto the family id; cache fields derived from input when absent
        "anthropic/claude-haiku-4-5-20251001": {"input_cost_per_token": 1e-6, "output_cost_per_token": 5e-6},
        "anthropic.claude-3-5-sonnet-20241022-v2:0": {
            "input_cost_per_token": 3e-6, "output_cost_per_token": 1.5e-5,
            "cache_read_input_token_cost": 3e-7, "cache_creation_input_token_cost": 3.75e-6,
        },
        "vertex_ai/claude-3-opus@20240229": {"input_cost_per_token": 1.5e-5, "output_cost_per_token": 7.5e-5},
        "claude-opus-4-8-20261001": {
            "input_cost_per_token": 5e-6, "output_cost_per_token": 2.5e-5,
            "cache_read_input_token_cost": 5e-7, "cache_creation_input_token_cost": 6.25e-6,
            "cache_creation_input_token_cost_above_1hr": 1e-5,
        },
        // junk
        "claude-no-output": {"input_cost_per_token": 1e-6},
        "claude-string-cost": {"input_cost_per_token": "0.000001", "output_cost_per_token": "0.000005"},
        "claude-negative": {"input_cost_per_token": -1e-6, "output_cost_per_token": 5e-6},
        "claude-huge": {"input_cost_per_token": 50.0, "output_cost_per_token": 50.0},
        "claude-null": null,
        "claude-array": [1, 2, 3],
        "claude-3-haiku-20240307": 7,
    })
    .to_string()
}

fn ok_fetch(text: String) -> impl Fn(&str) -> Result<String, String> {
    move |_| Ok(text.clone())
}

#[test]
fn fallback_prefix_match_picks_the_longest_key() {
    let t = PriceTable::fallback();
    let (r, s) = t.price_for("claude-opus-4-1");
    assert_eq!((r.input, r.output, s), (15.0, 75.0, Source::Fallback));
    let (r, _) = t.price_for("claude-opus-4-8");
    assert_eq!((r.input, r.output), (5.0, 25.0));
    // the generic `claude-sonnet` key and the specific 5.5 one agree; fable has its own row
    assert_eq!(
        t.price_for("claude-sonnet-5-5").0,
        Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)
    );
    assert_eq!(t.price_for("claude-fable-5").0.input, 10.0);
    // a haiku 3.5 id must not fall into the pricier haiku-4/5 rows
    assert_eq!(t.price_for("claude-3-5-haiku-20241022").0.input, 0.8);
    assert_eq!(t.price_for("claude-haiku-4-5").0.input, 1.0);
}

#[test]
fn prefix_matches_only_at_token_boundaries() {
    let t = PriceTable::fallback();
    // `claude-opus-4` must not match `claude-opus-45`; unknown -> default
    let (r, s) = t.price_for("claude-opus-45");
    assert_eq!(s, Source::Default);
    assert_eq!(r, t.default_rates());
    assert_eq!(t.price_for("totally-unknown").1, Source::Default);
    assert_eq!(t.price_for("").1, Source::Default);
}

#[test]
fn dated_and_prefixed_ids_resolve() {
    let t = PriceTable::fallback();
    for id in [
        "claude-sonnet-4-5-20250929",
        "anthropic/claude-sonnet-4-5",
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "claude-sonnet-4-5[1m]",
        "CLAUDE-SONNET-4-5",
        "claude-sonnet-4-5@20250929",
    ] {
        assert_eq!(t.price_for(id).0.input, 3.0, "{id}");
        assert_eq!(normalize_id(id), "claude-sonnet-4-5", "{id}");
    }
    assert_eq!(
        normalize_id("claude-3-5-sonnet-latest"),
        "claude-3-5-sonnet"
    );
    assert_eq!(
        normalize_id("claude-3-5-sonnet-v2@20241022"),
        "claude-3-5-sonnet"
    );
    assert_eq!(
        normalize_id("vertex_ai/claude-3-opus@20240229"),
        "claude-3-opus"
    );
}

#[test]
fn pretty_names() {
    for (id, want) in [
        ("claude-opus-4-8", "Opus 4.8"),
        ("claude-sonnet-5-5", "Sonnet 5.5"),
        ("claude-fable-5", "Fable 5"),
        ("claude-haiku-4-5-20251001", "Haiku 4.5"),
        ("claude-sonnet-4-20250514", "Sonnet 4"),
        ("claude-3-5-haiku-20241022", "Haiku 3.5"),
        ("claude-3-opus", "Opus 3"),
        ("claude-opus-4-8[1m]", "Opus 4.8"),
        ("anthropic/claude-opus-4-1", "Opus 4.1"),
        ("gpt-4o", "gpt-4o"),
        ("<synthetic>", "<synthetic>"),
    ] {
        assert_eq!(pretty_model(id), want, "{id}");
    }
}

#[test]
fn token_usage_cache_creation_split_and_total() {
    let split = TokenUsage::from_usage(&json!({
        "input_tokens": 3, "output_tokens": 4, "cache_read_input_tokens": 5,
        "cache_creation_input_tokens": 300,
        "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 200},
    }));
    assert_eq!((split.cache_write_5m, split.cache_write_1h), (100, 200));
    assert_eq!(split.total(), 3 + 4 + 5 + 300);

    let total_only = TokenUsage::from_usage(&json!({"cache_creation_input_tokens": 300}));
    assert_eq!(
        (total_only.cache_write_5m, total_only.cache_write_1h),
        (300, 0)
    );
    // an empty/zeroed split object falls back to the total at the 5 minute rate
    let zeroed = TokenUsage::from_usage(&json!({
        "cache_creation_input_tokens": 50,
        "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0},
    }));
    assert_eq!(zeroed.cache_write_5m, 50);
    assert_eq!(
        TokenUsage::from_usage(&json!("junk")),
        TokenUsage::default()
    );

    let mut sum = split;
    sum += total_only;
    assert_eq!(sum.cache_write_5m, 400);
}

#[test]
fn one_hour_cache_writes_cost_more_than_five_minute() {
    let t = PriceTable::fallback();
    let five = TokenUsage {
        cache_write_5m: 1_000_000,
        ..Default::default()
    };
    let hour = TokenUsage {
        cache_write_1h: 1_000_000,
        ..Default::default()
    };
    assert!(near(t.cost("claude-sonnet-5-5", &five), 3.75));
    assert!(near(t.cost("claude-sonnet-5-5", &hour), 6.0));
    assert!(near(t.cost("claude-opus-4-8", &five), 6.25));
    assert!(near(t.cost("claude-opus-4-8", &hour), 10.0));
}

#[test]
fn worked_example_matches_the_usage_panel() {
    // 48 in, 860 out, 2.4M cache read, 76.4k cache write at $3 / $15 / $0.30 / $3.75 per MTok:
    //   48 * 3 = 144; 860 * 15 = 12_900; 2_400_000 * 0.3 = 720_000; 76_400 * 3.75 = 286_500
    //   sum = 1_019_544 per-million-units = $1.019544   (the /usage panel showed $0.98)
    let u = TokenUsage {
        input: 48,
        output: 860,
        cache_read: 2_400_000,
        cache_write_5m: 76_400,
        cache_write_1h: 0,
    };
    let t = PriceTable::fallback();
    assert!(near(t.cost("claude-sonnet-5-5", &u), 1.019544));
    // the competing $2 / $10 / $0.10 / $2.50 rates would give 96 + 8_600 + 240_000 + 191_000
    let other = Rates::new(2.0, 10.0, 0.1, 2.5, 4.0);
    assert!(near(cost_at(&other, &u), 0.439696));
    assert!((t.cost("claude-sonnet-5-5", &u) - 0.98).abs() < 0.05);
    assert!((cost_at(&other, &u) - 0.98).abs() > 0.5);
}

#[test]
fn live_list_parsing_maps_ids_and_skips_junk() {
    let m = parse_live_list(&live_fixture()).unwrap();
    // bare key wins over `anthropic/` and bedrock spellings
    assert_eq!(m["claude-sonnet-5-5"], Rates::new(2.0, 10.0, 0.2, 2.5, 4.0));
    // dates / provider prefixes removed; missing cache fields derived from input
    assert_eq!(m["claude-haiku-4-5"], Rates::new(1.0, 5.0, 0.1, 1.25, 2.0));
    // bedrock and vertex spellings
    assert_eq!(m["claude-3-5-sonnet"].output, 15.0);
    assert_eq!(m["claude-3-opus"].input, 15.0);
    assert_eq!(m["claude-opus-4-8"], Rates::new(5.0, 25.0, 0.5, 6.25, 10.0));
    // skipped: non-claude, sample_spec, missing/negative/string/huge/non-object entries
    let mut keys: Vec<_> = m.keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "claude-3-5-sonnet",
            "claude-3-opus",
            "claude-haiku-4-5",
            "claude-opus-4-8",
            "claude-sonnet-5-5",
            "gpt-4o"
        ]
    );
    // OpenAI models (Codex) are read too, with derived cache fields
    assert_eq!(m["gpt-4o"].input, 2.5);
    assert_eq!(m["gpt-4o"].output, 10.0);
}

#[test]
fn live_list_parsing_never_panics_on_garbage() {
    assert!(parse_live_list("").is_err());
    assert!(parse_live_list("{not json").is_err());
    assert!(parse_live_list("[1,2]").is_err());
    assert!(parse_live_list("null").is_err());
    assert!(parse_live_list("{}").unwrap().is_empty());
    assert!(
        parse_live_list(r#"{"claude-x": {"input_cost_per_token": 1e999}}"#).is_err()
            || parse_live_list(r#"{"claude-x": {"input_cost_per_token": 1e999}}"#)
                .unwrap()
                .is_empty()
    );
    assert!(parse_live_list("\u{0}\u{1}").is_err());
}

#[test]
fn network_load_writes_cache_and_live_beats_fallback() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    let calls = Cell::new(0);
    let fetch = |url: &str| {
        calls.set(calls.get() + 1);
        assert_eq!(url, LIVE_URL);
        Ok(live_fixture())
    };
    let t = PriceTable::load_with(&paths, now(), true, &fetch);
    assert_eq!(calls.get(), 1);
    let info = t.info();
    assert_eq!(info.origin, ListOrigin::Network);
    assert_eq!(info.fetched_at, Some(now()));
    assert_eq!(info.age_secs, Some(0));
    assert!(!info.stale);
    assert_eq!(info.live_models, 6);
    assert!(info.fetch_error.is_none());
    // the live list beats the fallback ...
    let (r, s) = t.price_for("claude-opus-4-8-20261001");
    assert_eq!((r.input, r.output, s), (5.0, 25.0, Source::Live));
    // ... except where a built-in override pins the rate (sonnet-5-5: the list says $2/$10)
    let (r, s) = t.price_for("claude-sonnet-5-5-20261001");
    assert_eq!((r.input, r.output, s), (3.0, 15.0, Source::BuiltinOverride));
    // a model absent from the live list still resolves from the fallback
    assert_eq!(t.price_for("claude-fable-5").1, Source::Fallback);
    assert!(paths.cache.exists());
    assert!(t.info().summary().contains("live prices"));
}

#[test]
fn refresh_at_most_once_per_24_hours() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    let calls = Cell::new(0);
    let fetch = |_: &str| {
        calls.set(calls.get() + 1);
        Ok(live_fixture())
    };
    PriceTable::load_with(&paths, now(), true, &fetch);
    assert_eq!(calls.get(), 1);

    // 23h59m later: cache is fresh, no download
    let t = PriceTable::load_with(
        &paths,
        now() + Duration::minutes(23 * 60 + 59),
        true,
        &fetch,
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(t.info().origin, ListOrigin::Cache);
    assert!(!t.info().stale);
    assert_eq!(t.price_for("claude-opus-4-8").1, Source::Live);

    // exactly 24h: refreshed
    let t = PriceTable::load_with(&paths, now() + Duration::hours(24), true, &fetch);
    assert_eq!(calls.get(), 2);
    assert_eq!(t.info().origin, ListOrigin::Network);
    assert_eq!(t.info().fetched_at, Some(now() + Duration::hours(24)));

    // allow_network=false never downloads, even when due
    PriceTable::load_with(&paths, now() + Duration::hours(80), false, &fetch);
    assert_eq!(calls.get(), 2);
}

#[test]
fn stale_cache_is_used_when_the_download_fails() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    PriceTable::load_with(&paths, now(), true, &ok_fetch(live_fixture()));

    let later = now() + Duration::days(10);
    let calls = Cell::new(0);
    let offline = |_: &str| {
        calls.set(calls.get() + 1);
        Err::<String, _>("network error: unreachable".to_owned())
    };
    let t = PriceTable::load_with(&paths, later, true, &offline);
    assert_eq!(calls.get(), 1);
    let info = t.info();
    assert_eq!(info.origin, ListOrigin::Cache);
    assert!(info.stale);
    assert_eq!(info.age_secs, Some(10 * 24 * 3600));
    assert!(near(info.age_hours().unwrap(), 240.0));
    assert!(info.fetch_error.as_deref().unwrap().contains("unreachable"));
    assert_eq!(t.price_for("claude-opus-4-8").1, Source::Live);
    assert_eq!(t.price_for("claude-sonnet-5-5").1, Source::BuiltinOverride);
    assert!(info.summary().contains("stale"));
    assert!(info.summary().contains("10 d old"));

    // a failed attempt is not retried for an hour, and the cache survived the failed attempt
    let t = PriceTable::load_with(&paths, later + Duration::minutes(30), true, &offline);
    assert_eq!(calls.get(), 1);
    assert_eq!(t.info().origin, ListOrigin::Cache);
    let t = PriceTable::load_with(&paths, later + Duration::minutes(61), true, &offline);
    assert_eq!(calls.get(), 2);
    assert_eq!(t.price_for("claude-opus-4-8").1, Source::Live);
}

#[test]
fn offline_without_cache_uses_the_fallback() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    let offline = |_: &str| Err::<String, _>("HTTP 503".to_owned());
    let t = PriceTable::load_with(&paths, now(), true, &offline);
    let info = t.info();
    assert_eq!(info.origin, ListOrigin::Fallback);
    assert_eq!(info.fetched_at, None);
    assert_eq!(info.age_secs, None);
    assert_eq!(info.fetch_error.as_deref(), Some("HTTP 503"));
    assert_eq!(
        t.price_for("claude-sonnet-5-5"),
        (
            Rates::new(3.0, 15.0, 0.3, 3.75, 6.0),
            Source::BuiltinOverride
        )
    );
    assert_eq!(t.price_for("claude-haiku-4-5").1, Source::Fallback);
    assert!(info.summary().contains(AS_OF));

    // network disallowed: no attempt at all, no files written
    let dir2 = TempDir::new();
    let paths2 = Paths::in_dir(dir2.path());
    let never = |_: &str| -> Result<String, String> { panic!("must not download") };
    let t = PriceTable::load_with(&paths2, now(), false, &never);
    assert_eq!(t.info().origin, ListOrigin::Fallback);
    assert!(t.info().fetch_error.is_none());
    assert!(!paths2.cache.exists());
}

#[test]
fn unusable_downloads_are_failures() {
    for body in [
        "not json",
        "[]",
        "{}",
        r#"{"llama-4":{"input_cost_per_token":1e-6,"output_cost_per_token":1e-6}}"#,
    ] {
        let dir = TempDir::new();
        let paths = Paths::in_dir(dir.path());
        let t = PriceTable::load_with(&paths, now(), true, &ok_fetch(body.to_owned()));
        assert_eq!(t.info().origin, ListOrigin::Fallback, "{body}");
        assert!(t.info().fetch_error.is_some(), "{body}");
        assert_eq!(t.price_for("claude-haiku-4-5").1, Source::Fallback);
    }
}

#[test]
fn malformed_cache_and_overrides_are_ignored_with_warnings() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    std::fs::write(&paths.cache, "{ truncated").unwrap();
    std::fs::write(&paths.overrides, "[1, 2").unwrap();
    let t = PriceTable::load_with(&paths, now(), false, &ok_fetch(String::new()));
    assert_eq!(t.info().origin, ListOrigin::Fallback);
    assert_eq!(t.info().warnings.len(), 2);
    assert_eq!(t.price_for("claude-haiku-4-5").1, Source::Fallback);

    // with the network allowed the broken cache is replaced by a good one
    let t = PriceTable::load_with(&paths, now(), true, &ok_fetch(live_fixture()));
    assert_eq!(t.info().origin, ListOrigin::Network);
    let t = PriceTable::load_with(&paths, now(), false, &ok_fetch(String::new()));
    assert_eq!(t.info().origin, ListOrigin::Cache);
    assert_eq!(t.info().warnings.len(), 1); // the overrides file is still bad
}

#[test]
fn overrides_win_over_live_and_fallback() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    std::fs::write(
        &paths.overrides,
        json!({
            "_comment": "ignored",
            "claude-sonnet-5-5": {"input": 4, "output": 20, "cache_read": 0.4, "cache_write": 5, "cache_write_1h": 8},
            "claude-opus-4-8": {"input": 6, "output": 30},
            "broken": {"input": "x"},
            "default": {"input": 1, "output": 2, "cache_read": 0.1, "cache_write": 1.2, "cache_write_1h": 2},
        })
        .to_string(),
    )
    .unwrap();
    let t = PriceTable::load_with(&paths, now(), true, &ok_fetch(live_fixture()));
    assert_eq!(t.info().override_models, 2);
    assert_eq!(t.info().warnings.len(), 1); // the broken entry
    let (r, s) = t.price_for("claude-sonnet-5-5-20261001");
    assert_eq!(
        (r, s),
        (Rates::new(4.0, 20.0, 0.4, 5.0, 8.0), Source::Override)
    );
    // missing cache fields are derived from `input`
    let (r, s) = t.price_for("claude-opus-4-8");
    assert_eq!(s, Source::Override);
    assert_eq!(r, Rates::new(6.0, 30.0, 0.6, 7.5, 12.0));
    // other models still use the live list; the override `default` replaces the built-in one
    assert_eq!(t.price_for("claude-haiku-4-5").1, Source::Live);
    let (r, s) = t.price_for("mystery-model");
    assert_eq!((r.input, s), (1.0, Source::Default));
    assert!(t.info().summary().contains("2 override"));

    // overrides also apply offline and without any cache
    let dir2 = TempDir::new();
    let p2 = Paths::in_dir(dir2.path());
    std::fs::copy(&paths.overrides, &p2.overrides).unwrap();
    let t = PriceTable::load_with(&p2, now(), false, &ok_fetch(String::new()));
    assert_eq!(t.price_for("claude-sonnet-5-5").1, Source::Override);
}

#[test]
fn cost_uses_cache_split_from_transcript_usage() {
    let t = PriceTable::fallback();
    let u = TokenUsage::from_usage(&json!({
        "input_tokens": 10, "output_tokens": 100,
        "cache_read_input_tokens": 1_000_000,
        "cache_creation_input_tokens": 2_000_000,
        "cache_creation": {"ephemeral_5m_input_tokens": 500_000, "ephemeral_1h_input_tokens": 1_500_000},
    }));
    // opus 4.8: 10*5 + 100*25 + 1M*0.5 + 0.5M*6.25 + 1.5M*10 = 50 + 2500 + 500_000 + 3_125_000 + 15_000_000
    assert!(near(t.cost("claude-opus-4-8", &u), 18.62755));
}

// ---- built-in override (Sonnet 5.5 pinned to Claude Code's own figures) -------------------------

fn worked_example() -> TokenUsage {
    TokenUsage {
        input: 48,
        output: 860,
        cache_read: 2_400_000,
        cache_write_5m: 76_400,
        cache_write_1h: 0,
    }
}

#[test]
fn builtin_override_beats_a_live_list_that_says_2_10_for_sonnet_5_5() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    // the live fixture really does say $2 / $10 / $0.10 for claude-sonnet-5-5
    let t = PriceTable::load_with(&paths, now(), true, &ok_fetch(live_fixture()));
    assert_eq!(t.info().origin, ListOrigin::Network);
    let (r, s) = t.price_for("claude-sonnet-5-5");
    assert_eq!(
        (r, s),
        (
            Rates::new(3.0, 15.0, 0.3, 3.75, 6.0),
            Source::BuiltinOverride
        )
    );
    assert!(near(
        t.cost("claude-sonnet-5-5", &worked_example()),
        1.019544
    ));
    assert!(near(
        t.cost("claude-sonnet-5-5-20261001", &worked_example()),
        1.019544
    ));
    // also from a cached copy of that list, offline
    let t = PriceTable::load_with(
        &paths,
        now() + Duration::hours(2),
        false,
        &ok_fetch(String::new()),
    );
    assert_eq!(t.info().origin, ListOrigin::Cache);
    assert!(near(
        t.cost("claude-sonnet-5-5", &worked_example()),
        1.019544
    ));
    assert!(
        t.info().summary().contains("1 built-in override"),
        "{}",
        t.info().summary()
    );
    assert_eq!(t.info().builtin_override_models, 1);
}

#[test]
fn a_user_override_still_beats_the_builtin_one() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    std::fs::write(
        &paths.overrides,
        json!({"claude-sonnet-5-5": {"input": 1, "output": 5, "cache_read": 0.1, "cache_write": 1.25, "cache_write_1h": 2}}).to_string(),
    )
    .unwrap();
    let t = PriceTable::load_with(&paths, now(), true, &ok_fetch(live_fixture()));
    let (r, s) = t.price_for("claude-sonnet-5-5");
    assert_eq!((r.input, s), (1.0, Source::Override));
    assert!(near(
        t.cost("claude-sonnet-5-5", &worked_example()),
        0.339_848
    ));
}

#[test]
fn other_models_still_follow_the_live_list() {
    let dir = TempDir::new();
    let paths = Paths::in_dir(dir.path());
    let t = PriceTable::load_with(&paths, now(), true, &ok_fetch(live_fixture()));
    for (model, want) in [
        ("claude-opus-4-8", Rates::new(5.0, 25.0, 0.5, 6.25, 10.0)),
        ("claude-haiku-4-5", Rates::from_base(1.0, 5.0)),
        ("claude-3-5-sonnet", Rates::new(3.0, 15.0, 0.3, 3.75, 6.0)),
    ] {
        let (r, s) = t.price_for(model);
        assert_eq!((r, s), (want, Source::Live), "{model}");
    }
    // and a model the list lacks falls back as before
    assert_eq!(t.price_for("claude-fable-5").1, Source::Fallback);
    // the pinned table has exactly the one entry
    assert_eq!(aitrack::pricing::BUILTIN_OVERRIDES.len(), 1);
}
