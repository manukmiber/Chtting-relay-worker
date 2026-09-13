//! The two kinds of client key, and what a key owes.
//!
//! Everything here drives real HTTP against a real relay in front of the mock
//! backend, so what the assertions read is what a backend and a customer would
//! actually see.

mod common;

use common::{harness, MockConfig};
use serde_json::{json, Value};

use chtting_relay::config::{ClientKey, KeyKind, Model, Pricing};
use chtting_relay::store::invoice::{self, IssueRequest, Issued};

const PRIVATE_KEY: &str = "Kunci-Zeiko-private-test-key";
const COMPANY_KEY: &str = "Kunci-Zeiko-company-test-key";

/// A relay with one company key and one private key, and a price on the model
/// so that there is something to bill.
async fn priced_harness() -> common::Harness {
    harness(
        MockConfig {
            reply: "the answer to the question".into(),
            ..Default::default()
        },
        |cfg| {
            cfg.pricing = Pricing {
                enabled: true,
                backend_input_usd_per_m: 1.0,
                backend_output_usd_per_m: 2.0,
                input_usd_per_m: 10.0,
                output_usd_per_m: 20.0,
                ..Default::default()
            };
            cfg.keys.push(ClientKey {
                id: "key_company".into(),
                label: "acme".into(),
                key: COMPANY_KEY.into(),
                kind: KeyKind::Company,
                ..Default::default()
            });
            cfg.keys.push(ClientKey {
                id: "key_private".into(),
                label: "just me".into(),
                key: PRIVATE_KEY.into(),
                kind: KeyKind::Private,
                ..Default::default()
            });
        },
    )
    .await
}

fn ask() -> Value {
    json!({
        "model": "manukmiberai/creative-writer",
        "messages": [{"role": "user", "content": "what is the answer"}],
    })
}

/* ----------------------------------------------------- who goes upstream -- */

/// A company key resells the relay, so the end user it names is the one the
/// backend is told about — that is what keeps one customer's prompt cache out
/// of another's.
#[tokio::test]
async fn a_company_key_forwards_the_end_user_it_was_given() {
    let h = priced_harness().await;

    let mut body = ask();
    body["user"] = json!("their-customer-42");
    let response = h.post_as(COMPANY_KEY, "/v1/chat/completions", body).await;
    assert_eq!(response.status(), 200);

    let sent = h.backend.received.lock().last().cloned().unwrap();
    assert_eq!(sent["user"], "their-customer-42");
    assert_eq!(sent["user_id"], "their-customer-42");

    let row = h.last_row().await;
    assert_eq!(row["user_id"], "their-customer-42");
    assert_eq!(row["key_kind"], "company");
}

/// A private key is one person. Whatever they put in `user` is not a claim the
/// relay honours: they cannot file their spend under another name, and they
/// cannot land in another caller's cache partition.
#[tokio::test]
async fn a_private_key_is_its_own_user_whatever_the_caller_claims() {
    let h = priced_harness().await;

    let mut body = ask();
    body["user"] = json!("their-customer-42");
    let response = h.post_as(PRIVATE_KEY, "/v1/chat/completions", body).await;
    assert_eq!(response.status(), 200);

    let sent = h.backend.received.lock().last().cloned().unwrap();
    let upstream = sent["user"].as_str().unwrap();
    assert_ne!(
        upstream, "their-customer-42",
        "the claim must not be honoured"
    );
    assert_eq!(upstream, chtting_relay::util::fingerprint(PRIVATE_KEY));
    assert!(
        !upstream.contains(PRIVATE_KEY),
        "the key itself must never reach the backend",
    );

    let row = h.last_row().await;
    assert_eq!(row["key_kind"], "private");
    assert_eq!(row["user_id"], upstream);
}

/// The same for the header spelling: a private key cannot be talked out of its
/// own identity by any route in.
#[tokio::test]
async fn a_private_keys_identity_survives_a_user_header_too() {
    let h = priced_harness().await;

    let response = h
        .client()
        .post(h.url("/v1/chat/completions"))
        .header("authorization", format!("Bearer {PRIVATE_KEY}"))
        .header("x-user-id", "somebody-else")
        .json(&ask())
        .send()
        .await
        .expect("relay is reachable");
    assert_eq!(response.status(), 200);

    let sent = h.backend.received.lock().last().cloned().unwrap();
    assert_eq!(sent["user"], chtting_relay::util::fingerprint(PRIVATE_KEY));
}

/* --------------------------------------------------------------- pacing -- */

/// The throttle exists so one reseller's traffic does not fill the phone's
/// uplink at everybody else's expense. A private key has nobody else behind it,
/// so it is not held back — and the row says so, rather than claiming a ceiling
/// that was never applied.
#[tokio::test]
async fn a_private_key_is_never_held_to_the_models_tokens_per_second() {
    let h = harness(
        MockConfig {
            reply: "a reply long enough that pacing would show".into(),
            ..Default::default()
        },
        |cfg| {
            cfg.models[0].max_tokens_per_second = 5.0;
            cfg.keys.push(ClientKey {
                id: "key_private".into(),
                key: PRIVATE_KEY.into(),
                kind: KeyKind::Private,
                ..Default::default()
            });
        },
    )
    .await;

    let started = std::time::Instant::now();
    let response = h
        .post_as(PRIVATE_KEY, "/v1/chat/completions", {
            let mut body = ask();
            body["stream"] = json!(true);
            body
        })
        .await;
    assert_eq!(response.status(), 200);
    let (_events, _text) = common::read_sse(response).await;
    let unpaced = started.elapsed();

    let row = h.last_row().await;
    assert_eq!(row["target_tps"], 0.0, "a private key is not paced");
    assert_eq!(row["key_kind"], "private");

    // 44 characters at 5 tokens a second would be about two seconds. The
    // assertion is deliberately loose — this is "was the throttle applied at
    // all", not a timing benchmark.
    assert!(
        unpaced < std::time::Duration::from_millis(1500),
        "a private key waited {unpaced:?}, so something paced it",
    );

    // The same model, called with the ordinary company key, is still paced.
    let response = h
        .post("/v1/chat/completions", {
            let mut body = ask();
            body["stream"] = json!(true);
            body
        })
        .await;
    let _ = common::read_sse(response).await;
    let row = h.last_row().await;
    assert_eq!(
        row["target_tps"], 5.0,
        "a company key still keeps the route's ceiling",
    );
}

/* ---------------------------------------------------------- the ledger -- */

/// What a request cost has to be in the ledger, or none of the billing above
/// it means anything.
#[tokio::test]
async fn the_ledger_records_the_price_the_user_and_the_kind() {
    let h = priced_harness().await;
    let response = h.post_as(PRIVATE_KEY, "/v1/chat/completions", ask()).await;
    assert_eq!(response.status(), 200);

    let rows = h.ledger().await;
    let input = rows.iter().find(|r| r["phase"] == "input").unwrap();
    let closing = rows.iter().find(|r| r["phase"] == "final").unwrap();

    // Both rows say who and which kind; only the closing one carries money,
    // because until the answer is complete there is no price to record.
    for row in [input, closing] {
        assert_eq!(row["key_id"], "key_private");
        assert_eq!(row["key_kind"], "private");
        assert_eq!(
            row["user_id"],
            chtting_relay::util::fingerprint(PRIVATE_KEY)
        );
        assert_eq!(row["fmt"], 2, "new rows are written in the current format");
    }
    assert_eq!(input["proxy_usd"], 0.0);
    assert!(
        closing["proxy_usd"].as_f64().unwrap() > 0.0,
        "a priced request must land on the books with its price",
    );
    assert!(closing["backend_usd"].as_f64().unwrap() > 0.0);
    assert!(
        closing["proxy_usd"].as_f64().unwrap() > closing["backend_usd"].as_f64().unwrap(),
        "the sell rate here is ten times the cost rate",
    );

    // And the chain still holds over rows written in the new format.
    let check = h
        .state
        .store
        .read(|conn| Ok(chtting_relay::store::ledger::verify(conn)?))
        .await
        .unwrap();
    assert!(check.ok, "{}", check.message);
}

/* --------------------------------------------------------- invoicing -- */

/// The requirement in one test: usage accumulates per key, an invoice bills it
/// with a per-model breakdown, and afterwards the key's current usage reads
/// zero without a single recorded figure having been deleted.
#[tokio::test]
async fn an_invoice_bills_the_period_and_the_next_one_starts_empty() {
    let h = priced_harness().await;
    // A second model, so the invoice has more than one line to break down.
    h.state
        .config
        .upsert(
            "models",
            serde_json::to_value(Model {
                id: "manukmiberai/fast".into(),
                backend: "mock".into(),
                upstream_model: "Deepseek-v4-flash-0731".into(),
                enabled: true,
                ..Default::default()
            })
            .unwrap(),
        )
        .await
        .unwrap();

    for _ in 0..2 {
        h.post_as(COMPANY_KEY, "/v1/chat/completions", ask()).await;
    }
    let mut other = ask();
    other["model"] = json!("manukmiberai/fast");
    h.post_as(COMPANY_KEY, "/v1/chat/completions", other).await;
    // Somebody else's traffic, which this invoice must not touch.
    h.post_as(PRIVATE_KEY, "/v1/chat/completions", ask()).await;
    h.state.store.flush().await;

    let before = h
        .state
        .store
        .read(|conn| Ok(invoice::current_usage(conn, "key_company")?))
        .await
        .unwrap();
    assert_eq!(before.requests, 3);
    assert!(before.subtotal_usd > 0.0);
    assert_eq!(before.lines.len(), 2, "two models were used");

    let key = h
        .state
        .config
        .current()
        .keys
        .iter()
        .find(|k| k.id == "key_company")
        .cloned()
        .unwrap();
    let billing = h.state.config.current().billing.clone();
    let issued = h
        .state
        .store
        .write(move |conn| {
            invoice::issue(
                conn,
                IssueRequest {
                    key: &key,
                    billing: &billing,
                    note: String::new(),
                    tax_percent: Some(11.0),
                    force: false,
                    tz: chrono_tz::UTC,
                },
            )
        })
        .await
        .unwrap();
    let Issued::Invoice(inv) = issued else {
        panic!("three priced requests must produce an invoice");
    };

    assert_eq!(inv.requests, 3);
    assert_eq!(inv.key_id, "key_company");
    assert_eq!(inv.lines.len(), 2);
    let lines: f64 = inv.lines.iter().map(|l| l.amount_usd).sum();
    assert!(
        (lines - inv.subtotal_usd).abs() < 1e-6,
        "the model breakdown must add up to the subtotal",
    );
    assert!((inv.total_usd - (inv.subtotal_usd * 1.11)).abs() < 1e-6);

    // The reset: nothing deleted, and the key owes nothing.
    let (after, lifetime, theirs) = h
        .state
        .store
        .read(|conn| {
            Ok((
                invoice::current_usage(conn, "key_company")?,
                invoice::lifetime_usage(conn, "key_company")?,
                invoice::current_usage(conn, "key_private")?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(after.requests, 0);
    assert_eq!(after.subtotal_usd, 0.0);
    assert_eq!(lifetime.requests, 3, "the history is still all there");
    assert_eq!(theirs.requests, 1, "another key's period is untouched");

    // The ledger it was drawn from is still intact and still refuses to change.
    let check = h
        .state
        .store
        .read(|conn| Ok(chtting_relay::store::ledger::verify(conn)?))
        .await
        .unwrap();
    assert!(check.ok, "{}", check.message);
    assert!(
        h.sqlite("DELETE FROM usage_ledger").await.is_err(),
        "an invoice must not make the ledger deletable",
    );

    // New traffic after the invoice lands in the new period.
    h.post_as(COMPANY_KEY, "/v1/chat/completions", ask()).await;
    h.state.store.flush().await;
    let next = h
        .state
        .store
        .read(|conn| Ok(invoice::current_usage(conn, "key_company")?))
        .await
        .unwrap();
    assert_eq!(next.requests, 1);
}
