//! End-to-end tests: real HTTP into the real relay, in front of a mock backend.

mod common;

use common::{harness, read_sse, stream_text, MockConfig};
use serde_json::json;

fn chat(content: &str) -> serde_json::Value {
    json!({
        "model": "manukmiberai/creative-writer",
        "messages": [{"role": "user", "content": content}],
    })
}

/* ------------------------------------------------- 5. name translation -- */

#[tokio::test]
async fn the_caller_never_sees_the_backend_name_and_the_backend_never_sees_the_alias() {
    let h = harness(MockConfig::default(), |_| {}).await;

    let response = h.post("/v1/chat/completions", chat("halo")).await;
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();

    // Outward: the alias.
    assert_eq!(body["model"], "manukmiberai/creative-writer");
    assert!(
        !body.to_string().contains("Deepseek-v4-flash-0731"),
        "the backend's model name leaked to the caller"
    );
    // Upstream: the real name.
    assert_eq!(h.backend.last_request()["model"], "Deepseek-v4-flash-0731");
}

#[tokio::test]
async fn the_model_list_shows_aliases_only() {
    let h = harness(MockConfig::default(), |_| {}).await;
    let body: serde_json::Value = h.get("/v1/models").await.json().await.unwrap();
    let text = body.to_string();
    assert!(text.contains("manukmiberai/creative-writer"));
    assert!(!text.contains("Deepseek-v4-flash-0731"));
}

#[tokio::test]
async fn an_alias_routes_to_the_same_model() {
    let h = harness(MockConfig::default(), |cfg| {
        cfg.models[0].aliases = vec!["writer".into()];
    })
    .await;

    let response = h
        .post(
            "/v1/chat/completions",
            json!({"model": "writer", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(h.backend.last_request()["model"], "Deepseek-v4-flash-0731");
}

#[tokio::test]
async fn an_unknown_model_is_a_404_not_a_relayed_call() {
    let h = harness(MockConfig::default(), |_| {}).await;
    let response = h
        .post(
            "/v1/chat/completions",
            json!({"model": "nope", "messages": []}),
        )
        .await;
    assert_eq!(response.status(), 404);
    assert_eq!(h.backend.request_count(), 0, "the backend must not be called");
}

/* ------------------------------------------------- 4/6. prompt + shape -- */

#[tokio::test]
async fn the_configured_system_prompt_is_injected_before_the_backend_sees_it() {
    let h = harness(MockConfig::default(), |cfg| {
        cfg.models[0].system_prompt = chtting_relay::config::SystemPromptSpec {
            mode: "replace".into(),
            text: "You are a creative writer. Never mention your provider.".into(),
            prompt_id: String::new(),
        };
    })
    .await;

    h.post("/v1/chat/completions", chat("who made you?")).await;

    let sent = h.backend.last_request();
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert!(messages[0]["content"]
        .as_str()
        .unwrap()
        .contains("creative writer"));
}

#[tokio::test]
async fn response_rewrites_scrub_the_provider_out_of_the_reply() {
    let mock = MockConfig {
        reply: "I am DeepSeek, made by DeepSeek AI.".into(),
        ..Default::default()
    };
    let h = harness(mock, |cfg| {
        cfg.models[0].response_transform.replace = Some(vec![chtting_relay::config::TextRule {
            pattern: "DeepSeek".into(),
            flags: Some("gi".into()),
            replacement: "Creative Writer".into(),
            literal: false,
        }]);
    })
    .await;

    let body: serde_json::Value = h
        .post("/v1/chat/completions", chat("who are you?"))
        .await
        .json()
        .await
        .unwrap();

    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(content, "I am Creative Writer, made by Creative Writer AI.");
}

#[tokio::test]
async fn a_rewrite_split_across_streamed_chunks_is_still_caught() {
    // The mock emits four characters per frame, so "DeepSeek" is guaranteed to
    // straddle a chunk boundary.
    let mock = MockConfig {
        reply: "ask DeepSeek about writing".into(),
        ..Default::default()
    };
    let h = harness(mock, |cfg| {
        cfg.models[0].response_transform.replace = Some(vec![chtting_relay::config::TextRule {
            pattern: "DeepSeek".into(),
            flags: Some("g".into()),
            replacement: "Writer".into(),
            literal: false,
        }]);
    })
    .await;

    let mut body = chat("who are you?");
    body["stream"] = json!(true);
    let response = h.post("/v1/chat/completions", body).await;
    assert_eq!(response.status(), 200);

    let (events, raw) = read_sse(response).await;
    assert_eq!(stream_text(&events), "ask Writer about writing");
    assert!(!raw.contains("DeepSeek"), "the provider name leaked mid-stream");
}

#[tokio::test]
async fn reasoning_can_be_stripped_from_both_shapes() {
    let mock = MockConfig {
        reply: "the answer".into(),
        reasoning: Some("secret chain of thought".into()),
        ..Default::default()
    };
    let h = harness(mock, |cfg| {
        cfg.models[0].response_transform.reasoning = Some("strip".into());
    })
    .await;

    let buffered = h.post("/v1/chat/completions", chat("think")).await;
    let text = buffered.text().await.unwrap();
    assert!(!text.contains("secret chain of thought"));

    let mut streamed = chat("think");
    streamed["stream"] = json!(true);
    let (_, raw) = read_sse(h.post("/v1/chat/completions", streamed).await).await;
    assert!(!raw.contains("secret chain of thought"));
}

#[tokio::test]
async fn a_non_streaming_backend_can_still_be_replayed_as_a_stream() {
    let mock = MockConfig {
        reply: "buffered answer".into(),
        never_streams: true,
        ..Default::default()
    };
    let h = harness(mock, |_| {}).await;

    let mut body = chat("hi");
    body["stream"] = json!(true);
    let response = h.post("/v1/chat/completions", body).await;

    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/event-stream; charset=utf-8"
    );
    let (events, _) = read_sse(response).await;
    assert_eq!(stream_text(&events), "buffered answer");
}

#[tokio::test]
async fn forcing_a_stream_upstream_still_returns_json_to_the_caller() {
    let h = harness(MockConfig::default(), |cfg| {
        cfg.models[0].request_transform.force_stream = Some(Some(true));
    })
    .await;

    // The caller asked for a whole body...
    let response = h.post("/v1/chat/completions", chat("hi")).await;
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "hello from the backend");

    // ...but we streamed upstream, so TTFT is measurable.
    assert_eq!(h.backend.last_request()["stream"], true);
    let row = h.last_row().await;
    assert!(
        row["ttft_ms"].as_f64().unwrap_or(0.0) > 0.0,
        "forcing a stream should make TTFT measurable, got {row}"
    );
}

/* ---------------------------------------------------- 2/7. accounting -- */

#[tokio::test]
async fn tokens_are_counted_locally_when_the_backend_stays_silent() {
    // Pin a built-in vocabulary so the count is exact without any download.
    let h = harness(MockConfig::default(), |cfg| {
        cfg.models[0].tokenizer = "o200k_base".into();
    })
    .await;
    h.post("/v1/chat/completions", chat("count these tokens please"))
        .await;

    let row = h.last_row().await;
    assert!(row["prompt_tokens"].as_i64().unwrap() > 0);
    assert!(row["completion_tokens"].as_i64().unwrap() > 0);
    assert_eq!(row["usage_source"], "local");
    assert_eq!(row["exact"], 1, "a built-in vocabulary is exact");
    assert_eq!(row["tokenizer"], "o200k_base");
}

#[tokio::test]
async fn an_uninstalled_vocabulary_still_counts_but_says_it_is_an_estimate() {
    // The upstream name matches the `deepseek*` rule, whose vocabulary is not
    // installed here. The call must still work — and must not claim to be exact.
    let h = harness(MockConfig::default(), |_| {}).await;
    let response = h.post("/v1/chat/completions", chat("halo dunia")).await;
    assert_eq!(response.status(), 200);

    let row = h.last_row().await;
    assert!(row["prompt_tokens"].as_i64().unwrap() > 0, "no count at all");
    assert_eq!(row["exact"], 0, "an estimate must never be reported as exact");
    assert_eq!(row["tokenizer"], "estimate");
}

#[tokio::test]
async fn the_backends_own_usage_wins_and_the_difference_is_recorded_as_drift() {
    let mock = MockConfig {
        usage: Some(json!({"prompt_tokens": 111, "completion_tokens": 222})),
        ..Default::default()
    };
    let h = harness(mock, |_| {}).await;
    h.post("/v1/chat/completions", chat("hello")).await;

    let row = h.last_row().await;
    assert_eq!(row["prompt_tokens"], 111);
    assert_eq!(row["completion_tokens"], 222);
    assert_eq!(row["usage_source"], "upstream");
    // The local count is kept beside it so the gap is visible.
    assert!(row["local_prompt"].as_i64().unwrap() > 0);
    assert_ne!(row["drift_prompt"], 0);
}

#[tokio::test]
async fn a_streamed_call_records_ttft_generation_window_and_throughput() {
    let mock = MockConfig {
        reply: "one two three four five six seven eight".into(),
        delay_ms: 15,
        ..Default::default()
    };
    let h = harness(mock, |_| {}).await;

    let mut body = chat("go");
    body["stream"] = json!(true);
    let response = h.post("/v1/chat/completions", body).await;
    let _ = read_sse(response).await;

    let row = h.last_row().await;
    assert_eq!(row["status"], 200);
    assert_eq!(row["stream"], 1);
    assert!(row["ttft_ms"].as_f64().unwrap() > 0.0, "no TTFT recorded");
    assert!(row["total_ms"].as_f64().unwrap() >= row["ttft_ms"].as_f64().unwrap());
    assert!(
        row["tokens_per_sec"].as_f64().unwrap() > 0.0,
        "no throughput recorded: {row}"
    );
}

#[tokio::test]
async fn a_prompt_over_the_models_limit_is_refused_before_the_backend_is_called() {
    let h = harness(MockConfig::default(), |cfg| {
        cfg.models[0].limits.max_input_tokens = 5;
    })
    .await;

    let response = h
        .post(
            "/v1/chat/completions",
            chat("this prompt is comfortably longer than five tokens by any measure"),
        )
        .await;

    assert_eq!(response.status(), 413);
    assert_eq!(h.backend.request_count(), 0);
    let row = h.last_row().await;
    assert_eq!(row["status"], 413);
}

#[tokio::test]
async fn multibyte_replies_survive_being_torn_across_network_chunks() {
    // One byte at a time: the worst thing a mobile link can do to UTF-8.
    let mock = MockConfig {
        reply: "你好世界，这是一个中文测试。🚀".into(),
        byte_chunk_size: Some(1),
        ..Default::default()
    };
    let h = harness(mock, |_| {}).await;

    let mut body = chat("say something in chinese");
    body["stream"] = json!(true);
    let (events, _) = read_sse(h.post("/v1/chat/completions", body).await).await;

    let text = stream_text(&events);
    assert_eq!(text, "你好世界，这是一个中文测试。🚀");
    assert!(!text.contains('\u{FFFD}'), "UTF-8 was mangled: {text:?}");
}

/* ------------------------------------------------------ auth + quotas -- */

#[tokio::test]
async fn a_call_without_a_key_is_refused() {
    let h = harness(MockConfig::default(), |_| {}).await;
    let response = h
        .client()
        .post(h.url("/v1/chat/completions"))
        .json(&chat("hi"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(h.backend.request_count(), 0);
}

#[tokio::test]
async fn a_key_restricted_to_another_model_is_refused() {
    let h = harness(MockConfig::default(), |cfg| {
        cfg.keys[0].models = vec!["some-other-model".into()];
    })
    .await;
    let response = h.post("/v1/chat/completions", chat("hi")).await;
    assert_eq!(response.status(), 403);
    assert_eq!(h.backend.request_count(), 0);
}

#[tokio::test]
async fn a_per_minute_rate_limit_refuses_with_retry_after() {
    let h = harness(MockConfig::default(), |cfg| {
        cfg.keys[0].quota.requests_per_minute = 2;
    })
    .await;

    for i in 0..2 {
        let response = h.post("/v1/chat/completions", chat("hi")).await;
        assert_eq!(response.status(), 200, "call {i} should be allowed");
    }
    let refused = h.post("/v1/chat/completions", chat("hi")).await;
    assert_eq!(refused.status(), 429);
    assert!(refused.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn a_daily_token_quota_is_enforced_from_memory() {
    // The backend reports a large usage, so one call is enough to blow a
    // small daily budget regardless of how the prompt itself counts.
    let mock = MockConfig {
        usage: Some(json!({"prompt_tokens": 40, "completion_tokens": 10})),
        ..Default::default()
    };
    let h = harness(mock, |cfg| {
        cfg.keys[0].quota.tokens_per_day = 20;
    })
    .await;

    // The first call goes through and spends the budget.
    assert_eq!(
        h.post("/v1/chat/completions", chat("hello there")).await.status(),
        200
    );
    // Wait for the row, which is also when the quota counter is updated.
    let row = h.last_row().await;
    assert_eq!(row["total_tokens"], 50);

    let refused = h.post("/v1/chat/completions", chat("hello again")).await;
    assert_eq!(refused.status(), 429);
    let body: serde_json::Value = refused.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("daily token quota"));
}

#[tokio::test]
async fn a_daily_request_quota_is_enforced_too() {
    let h = harness(MockConfig::default(), |cfg| {
        cfg.keys[0].quota.requests_per_day = 1;
    })
    .await;

    assert_eq!(h.post("/v1/chat/completions", chat("one")).await.status(), 200);
    let _ = h.last_row().await;

    let refused = h.post("/v1/chat/completions", chat("two")).await;
    assert_eq!(refused.status(), 429);
    assert_eq!(h.backend.request_count(), 1, "the refused call never reached the backend");
}

/* --------------------------------------------------- failure handling -- */

#[tokio::test]
async fn a_backend_error_status_reaches_the_caller_rather_than_a_generic_502() {
    let mock = MockConfig {
        status: 503,
        error_body: Some(json!({"error": {"message": "model is warming up"}}).to_string()),
        ..Default::default()
    };
    let h = harness(mock, |_| {}).await;

    let response = h.post("/v1/chat/completions", chat("hi")).await;
    assert_eq!(response.status(), 503, "the backend's own status must survive");
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("model is warming up"));

    let row = h.last_row().await;
    assert_eq!(row["status"], 503);
}

#[tokio::test]
async fn a_disabled_model_is_not_reachable() {
    let h = harness(MockConfig::default(), |cfg| {
        cfg.models[0].enabled = false;
    })
    .await;
    assert_eq!(
        h.post("/v1/chat/completions", chat("hi")).await.status(),
        404
    );
}

/* ------------------------------------------------------------- health -- */

#[tokio::test]
async fn health_reports_what_is_configured_without_needing_a_key() {
    let h = harness(MockConfig::default(), |_| {}).await;
    let response = h
        .client()
        .get(h.url("/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["models"], 1);
    assert_eq!(body["backends"], 1);
}

#[tokio::test]
async fn embeddings_get_the_same_translation_and_accounting() {
    let h = harness(MockConfig::default(), |_| {}).await;
    let response = h
        .post(
            "/v1/embeddings",
            json!({"model": "manukmiberai/creative-writer", "input": "embed this"}),
        )
        .await;
    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["model"], "manukmiberai/creative-writer");
    assert!(!body.to_string().contains("backend-embed-1"));
    assert_eq!(h.backend.last_request()["model"], "Deepseek-v4-flash-0731");

    let row = h.last_row().await;
    assert_eq!(row["endpoint"], "v1/embeddings");
    assert_eq!(row["prompt_tokens"], 7, "the backend's own count wins");
}

/* -------------------------------------------------------- concurrency -- */

#[tokio::test]
async fn hundreds_of_concurrent_callers_all_get_correct_answers() {
    let mock = MockConfig {
        reply: "concurrent reply".into(),
        delay_ms: 20,
        ..Default::default()
    };
    let h = harness(mock, |cfg| {
        cfg.server.max_concurrent_requests = 1024;
    })
    .await;

    const CALLERS: usize = 300;
    let started = std::time::Instant::now();
    let mut tasks = Vec::with_capacity(CALLERS);
    for i in 0..CALLERS {
        let url = h.url("/v1/chat/completions");
        let key = h.client_key.clone();
        let client = h.client();
        tasks.push(tokio::spawn(async move {
            let response = client
                .post(url)
                .header("authorization", format!("Bearer {key}"))
                .json(&json!({
                    "model": "manukmiberai/creative-writer",
                    "messages": [{"role": "user", "content": format!("request number {i}")}],
                }))
                .send()
                .await?;
            let status = response.status().as_u16();
            let body: serde_json::Value = response.json().await?;
            Ok::<_, reqwest::Error>((status, body))
        }));
    }

    let mut ok = 0;
    for task in tasks {
        let (status, body) = task.await.expect("task joins").expect("request completes");
        assert_eq!(status, 200);
        assert_eq!(body["choices"][0]["message"]["content"], "concurrent reply");
        assert_eq!(body["model"], "manukmiberai/creative-writer");
        ok += 1;
    }
    let elapsed = started.elapsed();

    assert_eq!(ok, CALLERS);
    assert_eq!(h.backend.request_count(), CALLERS);
    // Each call sleeps 20ms upstream. Serialised that would be 6s; anything
    // near that means the requests are not actually running concurrently.
    assert!(
        elapsed.as_millis() < 3000,
        "{CALLERS} concurrent calls took {elapsed:?}, which is not concurrent"
    );

    // Every one of them was recorded.
    let rows = h.rows().await;
    assert_eq!(rows.len(), CALLERS, "some metrics rows were lost");
}

#[tokio::test]
async fn past_the_concurrency_ceiling_the_relay_refuses_instead_of_queueing() {
    let mock = MockConfig {
        delay_ms: 400,
        ..Default::default()
    };
    let h = harness(mock, |cfg| {
        cfg.server.max_concurrent_requests = 2;
    })
    .await;

    let mut tasks = Vec::new();
    for _ in 0..12 {
        let url = h.url("/v1/chat/completions");
        let key = h.client_key.clone();
        let client = h.client();
        tasks.push(tokio::spawn(async move {
            client
                .post(url)
                .header("authorization", format!("Bearer {key}"))
                .json(&json!({
                    "model": "manukmiberai/creative-writer",
                    "messages": [{"role": "user", "content": "hi"}],
                }))
                .send()
                .await
                .map(|r| r.status().as_u16())
        }));
    }

    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap().unwrap());
    }

    let refused = statuses.iter().filter(|s| **s == 503).count();
    let served = statuses.iter().filter(|s| **s == 200).count();
    assert!(refused > 0, "nothing was shed: {statuses:?}");
    assert!(served > 0, "everything was shed: {statuses:?}");
    assert_eq!(served + refused, 12);
    assert!(
        h.state
            .stats
            .rejected_overload
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
}
