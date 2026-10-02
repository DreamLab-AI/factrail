//! The jev-compaction policy suite (agentbox `tests/config/jev-compaction-policy.test.mjs`),
//! ported case by case, plus the three-outcome decision and option resolution.

use factrail_policy::*;
use serde_json::{Map, Value, json};

fn opts(v: Value) -> Map<String, Value> {
    v.as_object().cloned().expect("an object")
}
fn config(v: Value) -> Config {
    Config::from_options(&opts(v))
}
fn tool(name: &str) -> ToolRef {
    ToolRef {
        tool: name.into(),
        skill: None,
    }
}
fn skill(name: &str) -> ToolRef {
    ToolRef {
        tool: "Skill".into(),
        skill: Some(name.into()),
    }
}
fn clean() -> TaintScan {
    scan_taint(&[tool("Read")], &Config::default())
}
fn dirty() -> TaintScan {
    scan_taint(&[tool("mcp__email-gateway__ask_email")], &Config::default())
}

// ── taint — email never leaves ───────────────────────────────────────────────

#[test]
fn every_email_gateway_tool_taints_by_prefix() {
    let c = Config::default();
    for t in [
        "mcp__email-gateway__ask_email",
        "mcp__email-gateway__fetch_email_raw",
        "mcp__email-gateway__refresh_inbox",
        "mcp__claude_ai_Gmail__authenticate",
    ] {
        assert!(taints_tool(&tool(t), &c), "{t}");
    }
}

#[test]
fn a_skill_load_of_email_search_taints_other_skills_do_not() {
    let c = Config::default();
    assert!(taints_tool(&skill("email-search"), &c));
    assert!(!taints_tool(&skill("diagrams-as-code"), &c));
    assert!(
        !taints_tool(&tool("Skill"), &c),
        "a Skill call naming nothing is clean"
    );
}

#[test]
fn ordinary_tools_and_other_mcp_servers_are_clean() {
    let c = Config::default();
    for t in [
        "Read",
        "Bash",
        "Edit",
        "mcp__claude-flow__memory_search",
        "mcp__codebase-memory__search_graph",
        "email",
    ] {
        assert!(!taints_tool(&tool(t), &c), "{t}");
    }
}

#[test]
fn prefix_is_anchored_at_the_start() {
    assert!(!taints_tool(
        &tool("mcp__other__mcp__email-gateway__x"),
        &Config::default()
    ));
}

#[test]
fn scan_finds_a_single_email_call_anywhere_pinned_or_not() {
    let c = Config::default();
    let clean: Vec<ToolRef> = (0..50).flat_map(|_| [tool("Read"), tool("Bash")]).collect();
    assert_eq!(scan_taint(&clean, &c), TaintScan::default());

    let mut dirty = clean.clone();
    dirty.insert(20, tool("mcp__email-gateway__ask_email"));
    let r = scan_taint(&dirty, &c);
    assert!(r.tainted);
    assert_eq!(r.count, 1);
    assert_eq!(r.sample, ["mcp__email-gateway__ask_email"]);

    let mut last = clean;
    last.push(skill("email-search"));
    assert!(
        scan_taint(&last, &c).tainted,
        "a taint in the pinned tail still taints"
    );
}

#[test]
fn scan_sample_is_distinct_first_seen_and_at_most_three() {
    let c = Config::default();
    let names = ["mcp__email-gateway__a", "mcp__email-gateway__b"];
    let tools: Vec<ToolRef> = [
        names[0],
        names[0],
        names[1],
        "mcp__email-gateway__c",
        "mcp__email-gateway__d",
    ]
    .into_iter()
    .map(tool)
    .collect();
    let r = scan_taint(&tools, &c);
    assert_eq!(r.count, 5);
    assert_eq!(
        r.sample,
        [
            "mcp__email-gateway__a",
            "mcp__email-gateway__b",
            "mcp__email-gateway__c"
        ]
    );
    assert!(!r.sticky);
}

#[test]
fn per_project_fences_add_to_the_rule() {
    let c = config(json!({ "taintTools": "mcp__email-gateway__, mcp__codebase-memory__" }));
    assert!(taints_tool(&tool("mcp__codebase-memory__search_graph"), &c));
    assert!(taints_tool(&tool("mcp__email-gateway__ask_email"), &c));
}

#[test]
fn malformed_tool_entries_never_fail() {
    let tools: Vec<ToolRef> = serde_json::from_value(
        json!([{}, { "tool": null }, { "tool": 7 }, { "tool": ["x"], "skill": {} }]),
    )
    .expect("lenient");
    assert_eq!(tools[2].tool, "7");
    assert!(!scan_taint(&tools, &Config::default()).tainted);
    assert!(!scan_taint(&[], &Config::default()).tainted);
}

// ── the switch ───────────────────────────────────────────────────────────────

#[test]
fn store_beats_manifest_default_nothing_set_means_on() {
    assert!(resolve_enabled(None, None));
    assert!(!resolve_enabled(None, Some(&json!(false))));
    assert!(!resolve_enabled(None, Some(&json!("off"))));
    assert!(resolve_enabled(Some(&json!(true)), Some(&json!(false))));
    assert!(!resolve_enabled(Some(&json!(false)), Some(&json!(true))));
    assert!(
        resolve_enabled(Some(&json!("off")), None),
        "a non-boolean store value is not a switch"
    );
    assert!(!resolve_enabled(None, Some(&json!("No"))));
    assert!(resolve_enabled(None, Some(&json!("yes"))));
    assert!(
        resolve_enabled(None, Some(&json!(0))),
        "only strings and booleans are read"
    );
}

#[test]
fn enabled_by_default_flows_through_config() {
    let c = config(json!({ "enabledByDefault": "false" }));
    assert!(!resolve_enabled(None, c.enabled_by_default.as_ref()));
    assert!(resolve_enabled(
        None,
        Config::default().enabled_by_default.as_ref()
    ));
}

#[test]
fn switch_args() {
    assert_eq!(SwitchCommand::parse("on"), SwitchCommand::On);
    assert_eq!(SwitchCommand::parse(" OFF "), SwitchCommand::Off);
    assert_eq!(SwitchCommand::parse("enable"), SwitchCommand::On);
    assert_eq!(SwitchCommand::parse("0"), SwitchCommand::Off);
    assert_eq!(SwitchCommand::parse(""), SwitchCommand::Status);
    assert_eq!(SwitchCommand::parse("status"), SwitchCommand::Status);
    assert_eq!(SwitchCommand::parse("maybe"), SwitchCommand::Help);
}

#[test]
fn list_option_accepts_arrays_comma_strings_and_falls_back() {
    assert_eq!(list_option(Some(&json!(["a", " b "])), &["z"]), ["a", "b"]);
    assert_eq!(list_option(Some(&json!("a, b,,")), &["z"]), ["a", "b"]);
    assert_eq!(list_option(Some(&json!("")), &["z"]), ["z"]);
    assert_eq!(list_option(None, DEFAULT_TAINT_TOOLS), DEFAULT_TAINT_TOOLS);
    assert_eq!(list_option(Some(&json!(42)), &["z"]), ["z"]);
    assert!(
        list_option(Some(&json!([])), &["z"]).is_empty(),
        "an explicit [] is honoured"
    );
}

// ── decide — three outcomes, named reasons ───────────────────────────────────

#[test]
fn switched_off_wins_over_everything() {
    for fallback in ["rules", "summary"] {
        for local in [true, false] {
            let c = config(json!({ "fallback": fallback, "backendLocal": local }));
            for taint in [clean(), dirty()] {
                for credentials in [true, false] {
                    let v = decide(false, credentials, &taint, &c);
                    assert_eq!(
                        v,
                        Verdict {
                            judge: Judge::Builtin,
                            reason: Reason::SwitchedOff,
                            detail: None
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn no_key_means_rules_not_a_request() {
    let v = decide(true, false, &clean(), &Config::default());
    assert_eq!(
        v,
        Verdict {
            judge: Judge::Rules,
            reason: Reason::NoKey,
            detail: None
        }
    );
}

#[test]
fn no_key_beats_taint() {
    let local = config(json!({ "backendLocal": true }));
    for c in [Config::default(), local] {
        assert_eq!(decide(true, false, &dirty(), &c).reason, Reason::NoKey);
    }
}

#[test]
fn tainted_means_rules_with_the_offending_tool_named() {
    let v = decide(true, true, &dirty(), &Config::default());
    assert_eq!(v.judge, Judge::Rules);
    assert_eq!(v.reason, Reason::Tainted);
    assert_eq!(
        v.detail.as_deref(),
        Some("1 call(s): mcp__email-gateway__ask_email")
    );
}

#[test]
fn clean_on_keyed_runs_the_model() {
    assert_eq!(
        decide(true, true, &clean(), &Config::default()),
        Verdict {
            judge: Judge::Model,
            reason: Reason::Ok,
            detail: None
        }
    );
}

#[test]
fn fallback_summary_restores_the_builtin_summary_with_the_same_reason() {
    let c = config(json!({ "fallback": "summary" }));
    let v = decide(true, true, &dirty(), &c);
    assert_eq!((v.judge, v.reason), (Judge::Builtin, Reason::Tainted));
    assert!(v.detail.is_some());
    let v = decide(true, false, &clean(), &c);
    assert_eq!((v.judge, v.reason), (Judge::Builtin, Reason::NoKey));
    assert_eq!(decide(true, true, &clean(), &c).judge, Judge::Model);
    assert_eq!(
        config(json!({ "fallback": "SUMMARY" })).fallback,
        Fallback::Summary
    );
    assert_eq!(
        config(json!({ "fallback": "nonsense" })).fallback,
        Fallback::Rules
    );
}

// ── backendLocal: the fence opens only for a declared-local judge ────────────

#[test]
fn tainted_plus_local_backend_runs_reported_ok_local_with_the_call_named() {
    let v = decide(
        true,
        true,
        &dirty(),
        &config(json!({ "backendLocal": true })),
    );
    assert_eq!(v.judge, Judge::Model);
    assert_eq!(
        v.reason,
        Reason::OkLocal,
        "a judged tainted session must not log as a clean one"
    );
    assert!(v.detail.unwrap().contains("mcp__email-gateway__ask_email"));
}

#[test]
fn tainted_plus_cloud_or_omitted_backend_stays_fenced() {
    for c in [config(json!({ "backendLocal": false })), Config::default()] {
        let v = decide(true, true, &dirty(), &c);
        assert_eq!((v.judge, v.reason), (Judge::Rules, Reason::Tainted));
    }
}

#[test]
fn only_true_or_the_exact_string_true_opens_the_fence() {
    // The resolver's rule: a real boolean, or the projector's literal string.
    assert!(config(json!({ "backendLocal": true })).backend_local);
    assert!(config(json!({ "backendLocal": "true" })).backend_local);
    for v in [
        Value::Null,
        json!(false),
        json!(0),
        json!(1),
        json!(""),
        json!("TRUE"),
        json!(" true"),
        json!("yes"),
        json!("on"),
        json!("local"),
        json!("http://systemone:8097/v1/systemone"),
        json!("127.0.0.1"),
        json!({}),
        json!([]),
        json!(["true"]),
    ] {
        let c = config(json!({ "backendLocal": v.clone() }));
        assert!(!c.backend_local, "backendLocal={v} must not open the fence");
        assert_eq!(
            decide(true, true, &dirty(), &c).reason,
            Reason::Tainted,
            "{v}"
        );
    }
}

#[test]
fn locality_is_never_inferred_from_the_endpoint() {
    for url in [
        "http://127.0.0.1:8097/v1",
        "http://localhost:8097/v1",
        "http://192.168.0.10:8084/v1",
    ] {
        let c = config(json!({ "baseUrl": url }));
        assert_eq!(c.base_url.as_deref(), Some(url));
        assert!(!c.backend_local, "{url}");
    }
}

#[test]
fn a_local_backend_does_not_resurrect_a_switched_off_or_keyless_session() {
    let c = config(json!({ "backendLocal": true }));
    assert_eq!(
        decide(false, true, &dirty(), &c).reason,
        Reason::SwitchedOff
    );
    assert_eq!(
        decide(false, true, &clean(), &c).reason,
        Reason::SwitchedOff
    );
    assert_eq!(decide(true, false, &dirty(), &c).reason, Reason::NoKey);
    assert_eq!(decide(true, false, &clean(), &c).reason, Reason::NoKey);
}

#[test]
fn a_clean_session_is_ok_on_either_backend() {
    for local in [true, false] {
        let v = decide(
            true,
            true,
            &clean(),
            &config(json!({ "backendLocal": local })),
        );
        assert_eq!(
            v,
            Verdict {
                judge: Judge::Model,
                reason: Reason::Ok,
                detail: None
            }
        );
    }
}

#[test]
fn backend_local_false_is_identical_to_absent() {
    let explicit = config(json!({ "backendLocal": false }));
    for taint in [clean(), dirty()] {
        for enabled in [true, false] {
            for credentials in [true, false] {
                assert_eq!(
                    decide(enabled, credentials, &taint, &Config::default()),
                    decide(enabled, credentials, &taint, &explicit)
                );
            }
        }
    }
}

#[test]
fn reason_and_judge_vocabulary() {
    let reasons = [
        (Reason::Ok, "ok"),
        (Reason::OkLocal, "ok-local"),
        (Reason::SwitchedOff, "switched-off"),
        (Reason::NoKey, "no-key"),
        (Reason::Tainted, "tainted"),
    ];
    for (r, s) in reasons {
        assert_eq!(r.as_str(), s);
        assert_eq!(r.to_string(), s);
    }
    assert_eq!(Judge::Model.as_str(), "model");
    assert_eq!(Judge::Rules.as_str(), "rules");
    assert_eq!(Judge::Builtin.as_str(), "builtin");
}

// ── sticky taint — a summary that absorbed email never leaves ────────────────

#[test]
fn a_clean_looking_post_summary_transcript_stays_tainted() {
    let summarised = scan_taint(&[], &Config::default());
    assert!(!summarised.tainted, "the leak: the scan alone sees nothing");
    let sticky = taint_record(&dirty(), 1);
    let merged = merge_taint(Some(&sticky), summarised);
    assert!(merged.tainted && merged.sticky);
    assert_eq!(merged.sample, ["mcp__email-gateway__ask_email"]);
    assert_eq!(
        decide(true, true, &merged, &Config::default()).reason,
        Reason::Tainted
    );
}

#[test]
fn a_fresh_taint_wins_no_record_and_a_clean_scan_stays_clean() {
    let summarised = TaintScan::default();
    assert!(merge_taint(None, dirty()).tainted);
    assert!(
        !merge_taint(None, dirty()).sticky,
        "a fresh scan is not sticky"
    );
    assert!(!merge_taint(None, summarised.clone()).tainted);
    assert!(!merge_taint(Some(&json!({ "tainted": false })), summarised.clone()).tainted);
    assert!(!merge_taint(Some(&json!({ "tainted": "true" })), summarised.clone()).tainted);
    assert!(
        !merge_taint(Some(&json!("garbage")), summarised).tainted,
        "a malformed record is not a taint"
    );
}

#[test]
fn a_sticky_record_without_detail_still_reads_sensibly() {
    let merged = merge_taint(Some(&json!({ "tainted": true })), TaintScan::default());
    assert_eq!(merged.count, 1);
    assert_eq!(merged.sample, ["(earlier in this session)"]);
    let merged = merge_taint(
        Some(&json!({ "tainted": true, "count": "4", "sample": ["a", "b", "c", "d"] })),
        TaintScan::default(),
    );
    assert_eq!(merged.count, 4);
    assert_eq!(merged.sample, ["a", "b", "c"]);
}

#[test]
fn the_email_skill_expanded_as_a_command_taints_bare_or_qualified() {
    let c = Config::default();
    assert!(skill_taints("email-search", &c));
    assert!(skill_taints("agentbox:email-search", &c));
    assert!(!skill_taints("email-search-extra", &c));
    assert!(!skill_taints("", &c));
}

#[test]
fn a_local_backend_still_opens_the_fence_for_a_sticky_taint() {
    let merged = merge_taint(Some(&taint_record(&dirty(), 1)), TaintScan::default());
    let v = decide(
        true,
        true,
        &merged,
        &config(json!({ "backendLocal": true })),
    );
    assert_eq!(v.reason, Reason::OkLocal);
}

#[test]
fn the_first_taint_record_is_written_once() {
    let record = taint_write(None, &dirty(), 7).expect("first sighting is recorded");
    assert_eq!(
        record,
        json!({ "tainted": true, "count": 1, "sample": ["mcp__email-gateway__ask_email"], "at": 7 })
    );
    assert_eq!(taint_write(Some(&record), &dirty(), 9), None);
    assert_eq!(taint_write(None, &clean(), 9), None);
    assert!(sticky_tainted(Some(&record)));
    assert!(!sticky_tainted(None));
}

#[test]
fn store_keys() {
    assert_eq!(taint_key("s1"), "taint:s1");
    assert_eq!(baseline_key("s1"), "baseline:s1");
}

#[test]
fn taint_scan_serialises_with_a_stable_shape() {
    assert_eq!(
        serde_json::to_value(dirty()).unwrap(),
        json!({ "tainted": true, "count": 1, "sample": ["mcp__email-gateway__ask_email"], "sticky": false })
    );
}

// ── trigger — tokens as well as percent, with hysteresis ─────────────────────

fn base() -> Config {
    config(json!({ "compactAtPercent": 60, "compactAtTokens": 180_000, "rearmTokens": 40_000 }))
}

#[test]
fn absolute_wins_on_a_large_window_percent_on_a_small_one() {
    assert_eq!(trigger_tokens(Some(1_000_000.0), &base()), Some(180_000.0));
    assert_eq!(trigger_tokens(Some(200_000.0), &base()), Some(120_000.0));
    assert_eq!(trigger_tokens(None, &base()), Some(180_000.0));
    let no_cap = config(json!({ "compactAtPercent": 60, "compactAtTokens": 0 }));
    assert_eq!(
        trigger_tokens(Some(1_000_000.0), &no_cap),
        Some(600_000.0),
        "0 disables the absolute cap"
    );
    let neither = config(json!({ "compactAtPercent": 0, "compactAtTokens": 0 }));
    assert_eq!(
        trigger_tokens(Some(1_000_000.0), &neither),
        None,
        "no trigger is infinity"
    );
    assert_eq!(trigger_tokens(Some(f64::NAN), &no_cap), None);
}

#[test]
fn below_the_trigger_nothing_happens_at_it_it_runs() {
    let w = Some(1_000_000.0);
    let v = should_compact(Some(179_999.0), None, w, None, &base());
    assert_eq!((v.run, v.reason), (false, TriggerReason::BelowThreshold));
    let v = should_compact(Some(180_000.0), None, w, None, &base());
    assert_eq!((v.run, v.reason), (true, TriggerReason::Threshold));
}

#[test]
fn a_compaction_that_left_context_above_the_trigger_does_not_rerun_every_turn() {
    let w = Some(1_000_000.0);
    let after = Some(190_000.0);
    let v = should_compact(Some(200_000.0), None, w, after, &base());
    assert_eq!(
        (v.run, v.reason, v.need),
        (false, TriggerReason::Hysteresis, Some(230_000.0))
    );
    assert!(should_compact(Some(230_000.0), None, w, after, &base()).run);
}

#[test]
fn rearm_gap_explicit_else_a_quarter() {
    assert_eq!(rearm_gap(Some(180_000.0), 40_000.0), 40_000.0);
    assert_eq!(rearm_gap(Some(180_000.0), 0.0), 45_000.0);
    assert_eq!(rearm_gap(Some(10.0), 0.0), 3.0, "rounded up");
    assert_eq!(rearm_gap(None, 0.0), 0.0);
}

#[test]
fn without_a_token_figure_percent_triggers_but_never_repeatedly() {
    let w = Some(1_000_000.0);
    let v = should_compact(None, Some(70.0), w, None, &base());
    assert_eq!((v.run, v.reason), (true, TriggerReason::Percent));
    let v = should_compact(None, Some(70.0), w, Some(150_000.0), &base());
    assert_eq!((v.run, v.reason), (false, TriggerReason::NoUsage));
    let v = should_compact(None, Some(50.0), w, None, &base());
    assert_eq!((v.run, v.reason), (false, TriggerReason::NoUsage));
    assert_eq!(
        should_compact(None, None, w, None, &base()).reason,
        TriggerReason::NoUsage
    );
}

#[test]
fn no_trigger_never_runs_and_serialises_infinity_as_null() {
    let neither = config(json!({ "compactAtPercent": 0, "compactAtTokens": 0 }));
    let v = should_compact(
        Some(5_000_000.0),
        None,
        Some(1_000_000.0),
        Some(1.0),
        &neither,
    );
    assert_eq!((v.run, v.reason), (false, TriggerReason::BelowThreshold));
    assert_eq!(
        serde_json::to_value(v).unwrap(),
        json!({ "run": false, "reason": "below-threshold", "threshold": null, "need": null })
    );
}

// ── cache-warm nudge ─────────────────────────────────────────────────────────

#[test]
fn ttl_explicit_wins_subscription_hour_api_key_five_minutes_unknown_hour() {
    let explicit = config(json!({ "cacheTtlSeconds": 900 }));
    assert_eq!(cache_ttl_seconds(2, true, &explicit), 900.0);
    let auto = Config::default();
    assert_eq!(cache_ttl_seconds(2, true, &auto), 3600.0);
    assert_eq!(cache_ttl_seconds(0, true, &auto), 300.0);
    assert_eq!(cache_ttl_seconds(0, false, &auto), 3600.0);
}

#[test]
fn delay_is_ttl_minus_margin_or_half_a_ttl_the_margin_would_swallow() {
    assert_eq!(nudge_delay_ms(3600.0, 300.0), 3_300_000);
    assert_eq!(nudge_delay_ms(300.0, 300.0), 150_000);
    assert_eq!(nudge_delay_ms(f64::NAN, 300.0), 0);
    assert_eq!(nudge_delay_ms(-10.0, -10.0), 0);
    assert_eq!(nudge_delay_ms(1.0005, 0.0), 1001, "rounded half up");
}

#[test]
fn armed_only_above_the_floor_and_once_grown_past_the_last_compaction() {
    assert!(!should_arm_nudge(Some(90_000.0), 100_000.0, None, 40_000.0));
    assert!(should_arm_nudge(Some(120_000.0), 100_000.0, None, 40_000.0));
    assert!(!should_arm_nudge(
        Some(120_000.0),
        100_000.0,
        Some(110_000.0),
        40_000.0
    ));
    assert!(should_arm_nudge(
        Some(150_000.0),
        100_000.0,
        Some(110_000.0),
        40_000.0
    ));
    assert!(!should_arm_nudge(None, 100_000.0, None, 40_000.0));
}

#[test]
fn mode_parsing_defaults_to_compact_off_and_notify_are_opt_outs() {
    assert_eq!(CacheWarm::parse(None), CacheWarm::Compact);
    assert_eq!(CacheWarm::parse(Some(&Value::Null)), CacheWarm::Compact);
    assert_eq!(CacheWarm::parse(Some(&json!("OFF"))), CacheWarm::Off);
    assert_eq!(CacheWarm::parse(Some(&json!(false))), CacheWarm::Off);
    assert_eq!(CacheWarm::parse(Some(&json!(0))), CacheWarm::Off);
    assert_eq!(CacheWarm::parse(Some(&json!("notify"))), CacheWarm::Notify);
    assert_eq!(CacheWarm::parse(Some(&json!(" nudge "))), CacheWarm::Notify);
    assert_eq!(
        CacheWarm::parse(Some(&json!("whatever"))),
        CacheWarm::Compact
    );
    assert_eq!(
        config(json!({ "cacheWarm": "none" })).cache_warm,
        CacheWarm::Off
    );
}

#[test]
fn per_session_store_keys_expire_after_thirty_days_other_keys_never() {
    let now = SESSION_STATE_TTL_MS * 2;
    let entries: Vec<(String, Value)> = vec![
        ("taint:old".into(), json!({ "at": 0 })),
        ("taint:new".into(), json!({ "at": now - 1000 })),
        ("baseline:junk".into(), json!("x")),
        (
            "baseline:string-at".into(),
            json!({ "at": (now - 1000).to_string() }),
        ),
        ("enabled".into(), json!(true)),
        ("last".into(), json!("note")),
    ];
    assert_eq!(
        expired_session_keys(&entries, now, SESSION_STATE_TTL_MS),
        ["taint:old", "baseline:junk"]
    );
    assert_eq!(SESSION_STATE_TTL_MS, 2_592_000_000);
}

// ── scope — what may reach a judge ───────────────────────────────────────────

#[test]
fn only_the_main_conversations_real_compactions_are_in_scope() {
    for trigger in ["manual", "auto", "plugin"] {
        assert_eq!(compact_scope(Some(trigger), None), Scope::Main, "{trigger}");
    }
    assert_eq!(compact_scope(None, None), Scope::Main);
    assert_eq!(compact_scope(Some("precompute"), None), Scope::Precompute);
    assert_eq!(compact_scope(Some("manual"), Some("a1")), Scope::Subagent);
    assert_eq!(
        compact_scope(Some("precompute"), Some("a1")),
        Scope::Subagent,
        "a fork's precompute is still the fork's"
    );
    assert_eq!(
        compact_scope(Some("precompute"), Some("")),
        Scope::Precompute,
        "an empty agent id names nobody"
    );
}

#[test]
fn only_a_completed_main_loop_answer_may_trigger() {
    assert!(turn_may_trigger(None, Some("answer")));
    for reason in [Some("aborted"), Some("error"), Some("refusal"), None] {
        assert!(!turn_may_trigger(None, reason), "{reason:?}");
    }
    assert!(!turn_may_trigger(Some("a1"), Some("answer")));
    assert!(turn_may_trigger(Some(""), Some("answer")));
}

// ── deadline ─────────────────────────────────────────────────────────────────

#[test]
fn the_deadline_defaults_to_fifteen_seconds() {
    assert_eq!(DEFAULT_COMPACTION_TIMEOUT_MS, 15_000);
    for v in [
        None,
        Some(json!(0)),
        Some(json!(-5)),
        Some(json!("soon")),
        Some(json!("inf")),
        Some(json!(true)),
    ] {
        assert_eq!(compaction_timeout_ms(v.as_ref()), 15_000, "{v:?}");
    }
    assert_eq!(compaction_timeout_ms(Some(&json!(2500))), 2500);
    assert_eq!(compaction_timeout_ms(Some(&json!(2500.2))), 2501);
    assert_eq!(
        compaction_timeout_ms(Some(&json!("9000"))),
        9000,
        "shell-projected numbers are read"
    );
    assert_eq!(Config::default().compaction_timeout_ms, 15_000);
}

// ── option resolution ────────────────────────────────────────────────────────

#[test]
fn defaults() {
    let c = Config::default();
    assert_eq!(c.enabled_by_default, None);
    assert_eq!(c.taint_tools, DEFAULT_TAINT_TOOLS);
    assert_eq!(c.taint_skills, DEFAULT_TAINT_SKILLS);
    assert!(!c.backend_local);
    assert_eq!(c.backend, Backend::SystemOne);
    assert_eq!((c.base_url, c.model, c.api_key), (None, None, None));
    assert_eq!(c.egress, Egress::Full);
    assert_eq!(c.fallback, Fallback::Rules);
    assert_eq!(c.keep_threshold, 0.5);
    assert_eq!(c.preserve_recent_messages, 6);
    assert_eq!(c.max_state_tokens, 25_000);
    assert_eq!(c.max_request_tokens, 30_000);
    assert_eq!(c.truncate_head_chars, 200);
    assert_eq!(c.compact_at_percent, 60.0);
    assert_eq!(c.compact_at_tokens, 180_000.0);
    assert_eq!(c.rearm_tokens, 40_000.0);
    assert_eq!(c.cache_warm, CacheWarm::Compact);
    assert_eq!(c.cache_warm_floor_tokens, 100_000.0);
    assert_eq!(c.cache_ttl_seconds, 0.0);
    assert_eq!(c.cache_ttl_margin_seconds, 300.0);
    assert!(c.save_full_outputs && c.record_decisions);
    assert_eq!(c.min_reduction_ratio, None);
}

#[test]
fn every_option_is_read_from_its_camel_case_name() {
    let c = config(json!({
        "enabledByDefault": false,
        "taintTools": ["mcp__a__"],
        "taintSkills": "s1, s2",
        "backendLocal": true,
        "backend": "rules",
        "baseUrl": "http://x/v1",
        "model": "m",
        "apiKey": "k",
        "egress": "metadata",
        "fallback": "summary",
        "keepThreshold": 0.7,
        "preserveRecentMessages": 3,
        "maxStateTokens": 1000,
        "maxRequestTokens": 2000,
        "truncateHeadChars": 50,
        "compactAtPercent": 70,
        "compactAtTokens": 100000,
        "rearmTokens": 0,
        "cacheWarm": "notify",
        "cacheWarmFloorTokens": 1,
        "cacheTtlSeconds": 600,
        "cacheTtlMarginSeconds": 60,
        "compactionTimeoutMs": 2000,
        "saveFullOutputs": false,
        "recordDecisions": "false",
        "minReductionRatio": 0.3,
    }));
    assert_eq!(c.enabled_by_default, Some(json!(false)));
    assert_eq!(c.taint_tools, ["mcp__a__"]);
    assert_eq!(c.taint_skills, ["s1", "s2"]);
    assert!(c.backend_local);
    assert_eq!(c.backend, Backend::Rules);
    assert_eq!(c.base_url.as_deref(), Some("http://x/v1"));
    assert_eq!(c.model.as_deref(), Some("m"));
    assert_eq!(c.api_key.as_deref(), Some("k"));
    assert_eq!(c.egress, Egress::Metadata);
    assert_eq!(c.fallback, Fallback::Summary);
    assert_eq!(c.keep_threshold, 0.7);
    assert_eq!(
        (
            c.preserve_recent_messages,
            c.max_state_tokens,
            c.max_request_tokens,
            c.truncate_head_chars
        ),
        (3, 1000, 2000, 50)
    );
    assert_eq!(
        (c.compact_at_percent, c.compact_at_tokens, c.rearm_tokens),
        (70.0, 100_000.0, 0.0)
    );
    assert_eq!(c.cache_warm, CacheWarm::Notify);
    assert_eq!(
        (
            c.cache_warm_floor_tokens,
            c.cache_ttl_seconds,
            c.cache_ttl_margin_seconds
        ),
        (1.0, 600.0, 60.0)
    );
    assert_eq!(c.compaction_timeout_ms, 2000);
    assert!(!c.save_full_outputs && !c.record_decisions);
    assert_eq!(c.min_reduction_ratio, Some(0.3));
}

#[test]
fn shell_projected_strings_are_read_as_numbers_and_flags() {
    let c = config(json!({
        "compactAtTokens": " 120000 ",
        "preserveRecentMessages": "4",
        "minReductionRatio": "0.2",
        "saveFullOutputs": "OFF",
        "recordDecisions": "yes",
    }));
    assert_eq!(c.compact_at_tokens, 120_000.0);
    assert_eq!(c.preserve_recent_messages, 4);
    assert_eq!(c.min_reduction_ratio, Some(0.2));
    assert!(!c.save_full_outputs);
    assert!(c.record_decisions);
}

#[test]
fn malformed_values_mean_the_default() {
    let c = config(json!({
        "compactAtTokens": "lots",
        "keepThreshold": "NaN",
        "preserveRecentMessages": -1,
        "maxStateTokens": {},
        "minReductionRatio": "inf",
        "saveFullOutputs": "perhaps",
        "model": "",
        "apiKey": 42,
        "backend": "gpt",
        "taintSkills": "  ",
    }));
    assert_eq!(c.compact_at_tokens, 180_000.0);
    assert_eq!(c.keep_threshold, 0.5);
    assert_eq!(c.preserve_recent_messages, 6);
    assert_eq!(c.max_state_tokens, 25_000);
    assert_eq!(c.min_reduction_ratio, None);
    assert!(c.save_full_outputs);
    assert_eq!((c.model, c.api_key), (None, None));
    assert_eq!(c.backend, Backend::SystemOne);
    assert_eq!(c.taint_skills, DEFAULT_TAINT_SKILLS);
}

#[test]
fn keywords_are_trimmed_and_case_insensitive_and_egress_fails_toward_less() {
    assert_eq!(config(json!({ "backend": " TEV " })).backend, Backend::Tev);
    assert_eq!(
        config(json!({ "backend": "SystemOne" })).backend,
        Backend::SystemOne
    );
    assert_eq!(config(json!({ "egress": "FULL" })).egress, Egress::Full);
    assert_eq!(config(json!({ "egress": "" })).egress, Egress::Full);
    assert_eq!(
        config(json!({ "egress": "metdata" })).egress,
        Egress::Metadata
    );
    for (b, s) in [
        (Backend::SystemOne, "systemone"),
        (Backend::Tev, "tev"),
        (Backend::Rules, "rules"),
    ] {
        assert_eq!(b.as_str(), s);
    }
    assert_eq!(Egress::Metadata.as_str(), "metadata");
    assert_eq!(Fallback::Summary.as_str(), "summary");
    assert_eq!(CacheWarm::Notify.as_str(), "notify");
    assert_eq!(Scope::Precompute.as_str(), "precompute");
    assert_eq!(SwitchCommand::Help.as_str(), "help");
    assert_eq!(TriggerReason::NoUsage.as_str(), "no-usage");
}
