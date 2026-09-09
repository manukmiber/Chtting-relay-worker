//! The dashboard API, driven over real HTTP the way the frontend drives it.

mod common;

use common::{harness, MockConfig};
use serde_json::{json, Value};
use std::net::SocketAddr;

struct Dash {
    addr: SocketAddr,
    relay: common::Harness,
    client: reqwest::Client,
}

impl Dash {
    async fn start<F>(customise: F) -> Self
    where
        F: FnOnce(&mut chtting_relay::config::Config),
    {
        let relay = harness(MockConfig::default(), customise).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = chtting_relay::server::dashboard::router(relay.state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            relay,
            // A cookie store, so signing in carries over between calls.
            client: reqwest::Client::builder()
                .cookie_store(true)
                .build()
                .unwrap(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }

    async fn get_json(&self, path: &str) -> Value {
        self.get(path).await.json().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn put(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .put(self.url(path))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// Put one real request through the relay, so there is something to report.
    async fn relay_call(&self) {
        let res = self
            .relay
            .post(
                "/v1/chat/completions",
                json!({
                    "model": "manukmiberai/creative-writer",
                    "messages": [{"role": "user", "content": "hello"}],
                }),
            )
            .await;
        assert_eq!(res.status(), 200, "the relay call should succeed");
        self.relay.state.store.flush().await;
    }
}

#[tokio::test]
async fn the_dashboard_shell_is_served_from_inside_the_binary() {
    let dash = Dash::start(|_| {}).await;

    let response = dash.get("/").await;
    assert_eq!(response.status(), 200);
    let html = response.text().await.unwrap();
    assert!(html.contains("<html") || html.contains("<!doctype") || html.contains("<div"));

    // Assets too, with sensible content types.
    let css = dash.get("/css/app.css").await;
    assert_eq!(css.status(), 200);
    assert!(css
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .starts_with("text/css"));

    let js = dash.get("/js/app.js").await;
    assert_eq!(js.status(), 200);
}

#[tokio::test]
async fn state_reports_everything_the_dashboard_needs_in_one_call() {
    let dash = Dash::start(|_| {}).await;
    let state = dash.get_json("/api/state").await;

    assert!(state["config"].is_object());
    assert!(state["tokenizers"]["installed"].is_array());
    assert_eq!(state["store"]["kind"], "sqlite");
    assert_eq!(state["runtime"]["runtime"], "rust");
    assert!(state["runtime"]["cores"].as_u64().unwrap() >= 1);
    assert!(state["profiles"]
        .as_array()
        .unwrap()
        .contains(&json!("deepseek")));
    assert_eq!(state["relay"]["port"], 0);
}

#[tokio::test]
async fn secrets_are_masked_everywhere_the_dashboard_can_see_them() {
    let dash = Dash::start(|_| {}).await;

    let config = dash.get_json("/api/config").await;
    let api_key = config["backends"][0]["apiKey"].as_str().unwrap();
    assert_ne!(
        api_key, "sk-backend-secret",
        "the backend key was sent in the clear"
    );
    assert!(api_key.contains('…') || api_key.contains('•'));

    let key = config["keys"][0]["key"].as_str().unwrap();
    assert_ne!(key, "sk-relay-test-key");
}

#[tokio::test]
async fn saving_a_form_that_echoes_a_mask_keeps_the_real_secret() {
    let dash = Dash::start(|_| {}).await;

    // Read the masked config, change something unrelated, save it back.
    let mut config = dash.get_json("/api/config").await;
    config["backends"][0]["name"] = json!("renamed backend");
    let response = dash.put("/api/config", config).await;
    assert_eq!(response.status(), 200);

    // The rename stuck...
    let after = dash.get_json("/api/config").await;
    assert_eq!(after["backends"][0]["name"], "renamed backend");
    // ...and the real key survived rather than becoming the mask.
    let live = dash.relay.state.config.current();
    assert_eq!(live.backends[0].api_key, "sk-backend-secret");
    assert_eq!(live.backends[0].name, "renamed backend");
}

#[tokio::test]
async fn a_model_alias_can_be_repointed_at_a_different_backend_model() {
    let dash = Dash::start(|_| {}).await;

    let models = dash.get_json("/api/models").await;
    let mut model = models[0].clone();
    model["upstreamModel"] = json!("Qwen3-Coder-NEXT");

    let response = dash.post("/api/models", model).await;
    assert_eq!(response.status(), 200);

    // The change is live immediately, without a restart.
    let live = dash.relay.state.config.current();
    assert_eq!(live.models[0].upstream_model, "Qwen3-Coder-NEXT");

    // ...and the relay uses it on the very next call.
    dash.relay
        .post(
            "/v1/chat/completions",
            json!({"model": "manukmiberai/creative-writer",
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(
        dash.relay.backend.last_request()["model"],
        "Qwen3-Coder-NEXT"
    );
}

#[tokio::test]
async fn an_invalid_change_is_refused_with_a_reason_instead_of_being_saved() {
    let dash = Dash::start(|_| {}).await;

    let models = dash.get_json("/api/models").await;
    let mut model = models[0].clone();
    model["backend"] = json!("no-such-backend");

    let response = dash.post("/api/models", model).await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown backend"));

    // The live config is untouched.
    assert_eq!(dash.relay.state.config.current().models[0].backend, "mock");
}

#[tokio::test]
async fn a_generated_key_is_shown_once_and_masked_afterwards() {
    let dash = Dash::start(|_| {}).await;

    let created: Value = dash
        .post("/api/keys/generate", json!({"label": "phone"}))
        .await
        .json()
        .await
        .unwrap();
    let secret = created["item"]["key"].as_str().unwrap().to_string();
    assert!(
        secret.starts_with("sk-relay-"),
        "unexpected key shape: {secret}"
    );

    // Listing it afterwards only ever shows the mask.
    let keys = dash.get_json("/api/keys").await;
    let listed = keys
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["label"] == "phone")
        .unwrap();
    assert_ne!(listed["key"], secret.as_str());

    // ...but the new key really works against the relay.
    let response = dash
        .relay
        .client()
        .post(dash.relay.url("/v1/chat/completions"))
        .header("authorization", format!("Bearer {secret}"))
        .json(&json!({"model": "manukmiberai/creative-writer",
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn deleting_an_item_removes_it() {
    let dash = Dash::start(|_| {}).await;
    let created: Value = dash
        .post("/api/keys/generate", json!({"label": "temporary"}))
        .await
        .json()
        .await
        .unwrap();
    let id = created["item"]["id"].as_str().unwrap();

    assert_eq!(dash.relay.state.config.current().keys.len(), 2);
    let response = dash
        .client
        .delete(dash.url(&format!("/api/keys/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(dash.relay.state.config.current().keys.len(), 1);
}

#[tokio::test]
async fn statistics_come_back_shaped_the_way_the_charts_expect() {
    let dash = Dash::start(|_| {}).await;
    dash.relay
        .post(
            "/v1/chat/completions",
            json!({"model": "manukmiberai/creative-writer",
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    let _ = dash.relay.last_row().await;

    let summary = dash.get_json("/api/stats/summary?range=7d").await;
    assert_eq!(summary["all"]["requests"], 1);
    assert_eq!(summary["today"]["requests"], 1);
    assert!(summary["all"]["total_tokens"].as_i64().unwrap() > 0);
    for field in ["avg_ttft_ms", "p95_ttft_ms", "avg_tps", "error_rate"] {
        assert!(summary["all"][field].is_number(), "{field} is missing");
    }

    let daily = dash.get_json("/api/stats/daily?days=7").await;
    assert_eq!(daily.as_array().unwrap().len(), 1);
    assert!(daily[0]["day"].is_string());

    let by_model = dash.get_json("/api/stats/by/model?range=7d").await;
    assert_eq!(by_model[0]["name"], "manukmiberai/creative-writer");

    let by_key = dash.get_json("/api/stats/by/key?range=7d").await;
    assert_eq!(by_key[0]["label"], "test", "key rows need a human label");

    // Only whitelisted columns are groupable.
    let refused = dash.get("/api/stats/by/ip; DROP TABLE requests").await;
    assert_eq!(refused.status(), 400);
}

#[tokio::test]
async fn the_request_log_can_be_listed_filtered_and_opened() {
    let dash = Dash::start(|_| {}).await;
    dash.relay
        .post(
            "/v1/chat/completions",
            json!({"model": "manukmiberai/creative-writer",
                   "messages": [{"role": "user", "content": "a distinctive phrase"}]}),
        )
        .await;
    let _ = dash.relay.last_row().await;

    let listed = dash.get_json("/api/requests?limit=10").await;
    assert_eq!(listed["total"], 1);
    let row = &listed["rows"][0];
    assert_eq!(row["public_model"], "manukmiberai/creative-writer");
    assert_eq!(
        row["upstream_model"], "Deepseek-v4-flash-0731",
        "the local log does keep the real backend name"
    );

    // Full-text search over the stored previews.
    let found = dash.get_json("/api/requests?q=distinctive").await;
    assert_eq!(found["total"], 1);
    let missing = dash
        .get_json("/api/requests?q=nowhere-in-any-preview")
        .await;
    assert_eq!(missing["total"], 0);

    let id = row["id"].as_str().unwrap();
    let single = dash.get_json(&format!("/api/requests/{id}")).await;
    assert_eq!(single["id"], id);
    assert_eq!(dash.get("/api/requests/nope").await.status(), 404);
}

#[tokio::test]
async fn the_tokenizer_playground_counts_text_and_messages() {
    let dash = Dash::start(|_| {}).await;

    let text: Value = dash
        .post(
            "/api/tokenizer/count",
            json!({"text": "halo dunia", "tokenizer": "o200k_base"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(text["mode"], "text");
    assert_eq!(text["resolved"]["tokenizer"], "o200k_base");
    assert!(text["exact"].as_bool().unwrap());
    assert!(text["count"].as_u64().unwrap() > 0);

    // The frontend renders each piece as {text, id, special}; a bare list of
    // strings makes the playground throw instead of drawing anything.
    let pieces = text["pieces"].as_array().unwrap();
    assert!(!pieces.is_empty());
    for piece in pieces {
        assert!(piece["text"].is_string(), "piece has no text: {piece}");
        assert!(piece["id"].is_i64(), "piece has no id: {piece}");
        assert!(
            piece["special"].is_boolean(),
            "piece has no special flag: {piece}"
        );
    }
    let joined: String = pieces.iter().filter_map(|p| p["text"].as_str()).collect();
    assert_eq!(joined, "halo dunia", "pieces must reconstruct the input");

    let messages: Value = dash
        .post(
            "/api/tokenizer/count",
            json!({
                "model": "manukmiberai/creative-writer",
                "tokenizer": "cl100k_base",
                "messages": [{"role": "user", "content": "halo"}],
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(messages["mode"], "messages");
    // A pinned vocabulary must be used verbatim, not run back through the
    // model-matching rules.
    assert_eq!(messages["resolved"]["tokenizer"], "cl100k_base");
    assert!(messages["breakdown"]["overhead"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn the_tokenizer_inventory_lists_what_is_built_in() {
    let dash = Dash::start(|_| {}).await;
    let inventory = dash.get_json("/api/tokenizer/inventory").await;

    let installed = inventory["installed"].as_array().unwrap();
    let builtin: Vec<&str> = installed
        .iter()
        .filter(|i| i["builtin"] == json!(true))
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(builtin.contains(&"o200k_base"));
    assert!(builtin.contains(&"cl100k_base"));

    let available = inventory["available"].as_array().unwrap();
    assert!(available.iter().any(|a| a["name"] == "deepseek"));
}

#[tokio::test]
async fn tunnel_status_is_reportable_before_anything_has_run() {
    let dash = Dash::start(|_| {}).await;
    let status = dash.get_json("/api/tunnel").await;
    assert_eq!(status["state"], "stopped");
    assert_eq!(status["mode"], "quick");
    assert!(status["cloudflared"]["installed"].is_boolean());

    let refused = dash.post("/api/tunnel/explode", json!({})).await;
    assert_eq!(refused.status(), 400);
}

#[tokio::test]
async fn a_password_locks_the_dashboard_until_you_sign_in() {
    let dash = Dash::start(|cfg| {
        cfg.dashboard.password = "hunter2".into();
    })
    .await;

    // Locked.
    assert_eq!(dash.get("/api/state").await.status(), 401);
    let session = dash.get_json("/api/session").await;
    assert_eq!(session["authenticated"], false);
    assert_eq!(session["passwordSet"], true);

    // A wrong password stays locked.
    assert_eq!(
        dash.post("/api/login", json!({"password": "wrong"}))
            .await
            .status(),
        401
    );
    assert_eq!(dash.get("/api/state").await.status(), 401);

    // The right one opens it, and the cookie carries over.
    assert_eq!(
        dash.post("/api/login", json!({"password": "hunter2"}))
            .await
            .status(),
        200
    );
    assert_eq!(dash.get("/api/state").await.status(), 200);

    // Signing out closes it again.
    assert_eq!(dash.post("/api/logout", json!({})).await.status(), 200);
    assert_eq!(dash.get("/api/state").await.status(), 401);
}

#[tokio::test]
async fn without_a_password_the_dashboard_is_open_on_loopback() {
    let dash = Dash::start(|_| {}).await;
    assert_eq!(dash.get("/api/state").await.status(), 200);
    let session = dash.get_json("/api/session").await;
    assert_eq!(session["passwordSet"], false);
    assert_eq!(session["authenticated"], true);
}

#[tokio::test]
async fn the_playground_round_trips_a_real_call_through_the_relay() {
    // The playground calls the relay on its configured port, so point the
    // config at the port the harness actually bound.
    let dash = Dash::start(|_| {}).await;
    let port = dash.relay.addr.port();
    dash.relay
        .state
        .config
        .update(json!({"server": {"port": port}}))
        .await
        .unwrap();

    let result: Value = dash
        .post(
            "/api/playground",
            json!({
                "model": "manukmiberai/creative-writer",
                "messages": [{"role": "user", "content": "hi"}],
            }),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(result["status"], 200, "playground call failed: {result}");
    assert_eq!(result["body"]["model"], "manukmiberai/creative-writer");
    assert_eq!(
        result["body"]["choices"][0]["message"]["content"],
        "hello from the backend"
    );
}

#[tokio::test]
async fn pruning_reports_how_many_rows_it_removed() {
    let dash = Dash::start(|_| {}).await;
    let result: Value = dash
        .post("/api/maintenance/prune", json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["removed"], 0, "nothing is old enough to prune yet");
}

/* ------------------------------------------------------- usage and queue -- */

#[tokio::test]
async fn the_usage_summary_is_totalled_from_the_ledger() {
    let d = Dash::start(|_| {}).await;
    d.relay_call().await;

    let summary: Value = d.get_json("/api/usage/summary?range=30d").await;
    let window = &summary["window"];
    assert_eq!(window["requests"].as_i64().unwrap(), 1);
    assert!(window["inputTokens"].as_i64().unwrap() > 0);
    assert!(window["outputTokens"].as_i64().unwrap() > 0);
    assert_eq!(window["completed"].as_i64().unwrap(), 1);
    // Every field the dashboard's Usage tab reads must be present.
    for key in [
        "billedInputTokens",
        "cachedTokens",
        "cacheHits",
        "cacheHitRate",
        "avgTtftMs",
        "avgTokensPerSec",
        "avgQueuedMs",
        "users",
        "errors",
        "inFlight",
    ] {
        assert!(window.get(key).is_some(), "usage summary is missing {key}");
    }
}

#[tokio::test]
async fn the_usage_totals_survive_pruning_the_request_log() {
    let d = Dash::start(|_| {}).await;
    d.relay_call().await;

    let before: Value = d.get_json("/api/usage/summary?range=30d").await;
    let tokens = before["all"]["inputTokens"].as_i64().unwrap();
    assert!(tokens > 0);

    // Wipe the browsable log the way the maintenance button does.
    d.relay
        .state
        .store
        .read(|conn| {
            conn.execute("DELETE FROM requests", [])?;
            Ok(())
        })
        .await
        .unwrap();

    let after: Value = d.get_json("/api/usage/summary?range=30d").await;
    assert_eq!(
        after["all"]["inputTokens"].as_i64().unwrap(),
        tokens,
        "the ledger must outlive the request rows"
    );
}

#[tokio::test]
async fn the_ledger_endpoint_reports_an_intact_chain() {
    let d = Dash::start(|_| {}).await;
    d.relay_call().await;

    let rows: Value = d.get_json("/api/usage/ledger?limit=10").await;
    assert_eq!(rows.as_array().unwrap().len(), 2);

    let check: Value = d.get_json("/api/usage/verify").await;
    assert_eq!(check["ok"], true);
    assert_eq!(check["rows"].as_i64().unwrap(), 2);
    assert!(check["brokenAt"].is_null());
}

#[tokio::test]
async fn the_queue_endpoint_reports_both_the_setting_and_the_live_state() {
    let d = Dash::start(|cfg| {
        cfg.server.max_concurrent_requests = 7;
        cfg.server.queue_capacity = 99;
        cfg.server.queue_timeout_ms = 12_345;
    })
    .await;

    let queue: Value = d.get_json("/api/queue").await;
    assert_eq!(queue["configured"]["maxConcurrentRequests"], 7);
    assert_eq!(queue["configured"]["queueCapacity"], 99);
    assert_eq!(queue["configured"]["queueTimeoutMs"], 12_345);
    assert_eq!(queue["live"]["limit"], 7);
    assert_eq!(queue["live"]["inFlight"], 0);
}

#[tokio::test]
async fn the_concurrency_limit_can_be_raised_from_the_dashboard_without_a_restart() {
    let d = Dash::start(|cfg| cfg.server.max_concurrent_requests = 2).await;

    let res = d
        .put(
            "/api/config",
            json!({"server": {"maxConcurrentRequests": 32, "queueCapacity": 500}}),
        )
        .await;
    assert_eq!(res.status(), 200);

    // The gate resizes on the next request rather than at startup.
    d.relay_call().await;
    let queue: Value = d.get_json("/api/queue").await;
    assert_eq!(queue["configured"]["maxConcurrentRequests"], 32);
    assert_eq!(queue["live"]["limit"], 32);
}

#[tokio::test]
async fn the_openrouter_preview_shows_what_would_be_published() {
    let d = Dash::start(|cfg| {
        cfg.openrouter.enabled = true;
        cfg.models[0].openrouter.listed = true;
        cfg.models[0].openrouter.pricing.prompt_usd = "0.0000006".into();
    })
    .await;

    let preview: Value = d.get_json("/api/openrouter/preview").await;
    assert_eq!(preview["enabled"], true);
    assert_eq!(preview["tokenRequired"], false);
    assert_eq!(preview["document"]["data"][0]["schema_version"], "2.4");
    assert!(!preview.to_string().contains("Deepseek-v4-flash-0731"));
}

#[tokio::test]
async fn a_zero_retention_claim_is_refused_while_prompts_are_being_stored() {
    let d = Dash::start(|cfg| {
        cfg.logging.store_bodies = "preview".into();
    })
    .await;

    let res = d
        .put(
            "/api/config",
            json!({"openrouter": {"enabled": true, "compliance": {"zdr": true}}}),
        )
        .await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("ZDR"),
        "unhelpful refusal: {body}"
    );
}

/* ---------------------------------------------------------------- setup -- */
//
// Nothing here calls `/api/service/restart` or `/api/service/stop`: both end
// the process they are called on, and the process they would be called on here
// is the test runner.

#[tokio::test]
async fn setup_lists_what_is_still_missing_before_the_relay_can_serve() {
    let d = Dash::start(|cfg| {
        // A relay with a backend and a model but nothing to authenticate with.
        cfg.keys.clear();
        cfg.security.require_client_key = true;
    })
    .await;

    let setup: Value = d.get_json("/api/setup").await;
    let steps = setup["steps"].as_array().expect("steps must be a list");
    let step = |id: &str| {
        steps
            .iter()
            .find(|s| s["id"] == id)
            .unwrap_or_else(|| panic!("no \"{id}\" step in {steps:?}"))
            .clone()
    };

    assert_eq!(step("backend")["ok"], true, "the harness has a backend");
    assert_eq!(step("model")["ok"], true, "the harness has a model");
    assert_eq!(step("key")["ok"], false, "a required key is missing");
    assert!(
        step("key")["detail"]
            .as_str()
            .unwrap_or("")
            .contains("No key"),
        "the missing key should say so: {}",
        step("key")["detail"]
    );

    // Every unfinished step has to say where it is fixed, or the screen is a
    // list of complaints rather than a checklist.
    for s in steps {
        assert!(
            s["fix"].as_str().is_some_and(|f| !f.is_empty()),
            "step {} has nowhere to go",
            s["id"]
        );
    }

    // And the phone itself, honestly reported on a machine that is not one.
    assert_eq!(setup["termux"], chtting_relay::system::termux());
    assert!(setup["service"]["installed"].is_boolean());
    assert!(setup["shortcuts"]["files"].as_array().is_some());
    assert_eq!(setup["packages"].as_array().unwrap().len(), 2);
    assert_eq!(setup["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn a_key_that_exists_finishes_the_step_that_asked_for_one() {
    let d = Dash::start(|_| {}).await;
    let setup: Value = d.get_json("/api/setup").await;
    let key = setup["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "key")
        .unwrap()
        .clone();
    assert_eq!(key["ok"], true, "the harness ships a client key");
}

#[tokio::test]
async fn the_dashboard_only_installs_the_packages_it_names() {
    let d = Dash::start(|_| {}).await;

    let res = d
        .post("/api/system/package", json!({"package": "curl; rm -rf /"}))
        .await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("not one of the packages"),
        "unhelpful refusal: {body}"
    );

    // The empty package name is the same refusal, not a crash.
    assert_eq!(d.post("/api/system/package", json!({})).await.status(), 400);
}

#[tokio::test]
async fn an_unknown_service_action_is_refused_by_name() {
    let d = Dash::start(|_| {}).await;
    let res = d.post("/api/service/reboot-the-phone", json!({})).await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("reboot-the-phone"),
        "the refusal should name what it refused: {body}"
    );
}

#[tokio::test]
async fn handing_over_to_a_service_that_was_never_installed_says_so() {
    // On a phone this would be a real handover; here there is no service, and
    // the point is that the answer explains itself rather than exiting.
    if chtting_relay::system::termux() {
        return;
    }
    let d = Dash::start(|_| {}).await;
    let res = d.post("/api/service/hand-over", json!({})).await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("$PREFIX") || message.contains("service"),
        "unhelpful refusal: {body}"
    );
}

#[tokio::test]
async fn the_setup_routes_are_behind_the_password_like_everything_else() {
    let d = Dash::start(|cfg| {
        cfg.dashboard.password = "hunter2".into();
    })
    .await;

    assert_eq!(d.get("/api/setup").await.status(), 401);
    assert_eq!(
        d.post("/api/service/install", json!({})).await.status(),
        401
    );
    assert_eq!(
        d.post("/api/system/wakelock", json!({"on": true}))
            .await
            .status(),
        401
    );
    assert_eq!(
        d.post("/api/system/package", json!({"package": "cloudflared"}))
            .await
            .status(),
        401
    );

    d.post("/api/login", json!({"password": "hunter2"})).await;
    assert_eq!(d.get("/api/setup").await.status(), 200);
}
