//! Turning usage into a bill, and starting the next period.
//!
//! The relay's usage ledger is append-only on purpose: a figure that has been
//! recorded has no legitimate way to change. That makes "reset the usage after
//! the invoice goes out" a question with exactly one honest answer — you do not
//! reset anything. You draw a line.
//!
//! ```text
//!   usage_ledger   ─── seq ───────────────────────────────────────────────►
//!     … 41  42  43 │ 44  45  46  47 │ 48  49  50 …
//!                  │                │
//!            invoice #1        invoice #2         "current usage"
//!            to_seq = 43       to_seq = 47        = everything past 47
//! ```
//!
//! An invoice records where the line was drawn and what was on the near side of
//! it. Everything after that line is the new period, so the key's current usage
//! reads as zero the moment the invoice is issued — without a single ledger row
//! being touched, and with every past period still reconstructible from the
//! same rows months later.
//!
//! Two consequences worth stating, because they are the behaviour rather than
//! the implementation:
//!
//! * **An invoice cannot be un-issued.** Voiding one marks it void and hands
//!   its period back to the next invoice, which then covers both.
//! * **Rows that arrive during issue are not lost.** The line is a `seq` that
//!   already exists, so anything committed afterwards simply lands in the next
//!   period.

use anyhow::{bail, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{BillingConfig, ClientKey};
use crate::util::round;

pub const CREATE_SQL: &str = "
CREATE TABLE IF NOT EXISTS invoices (
  id TEXT PRIMARY KEY,
  number TEXT NOT NULL,
  key_id TEXT NOT NULL,
  key_label TEXT NOT NULL,
  key_kind TEXT NOT NULL,
  bill_to TEXT NOT NULL,
  issued_at INTEGER NOT NULL,
  period_start INTEGER NOT NULL,
  period_end INTEGER NOT NULL,
  from_seq INTEGER NOT NULL,
  to_seq INTEGER NOT NULL,
  currency TEXT NOT NULL,
  requests INTEGER NOT NULL,
  input_tokens INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  cached_tokens INTEGER NOT NULL,
  reasoning_tokens INTEGER NOT NULL,
  subtotal_usd REAL NOT NULL,
  tax_percent REAL NOT NULL,
  tax_usd REAL NOT NULL,
  total_usd REAL NOT NULL,
  backend_usd REAL NOT NULL,
  lines TEXT NOT NULL,
  note TEXT NOT NULL,
  status TEXT NOT NULL,
  settled_at INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  due_at INTEGER NOT NULL DEFAULT 0,
  issuer TEXT NOT NULL DEFAULT '',
  idr_per_usdt REAL NOT NULL DEFAULT 0,
  usd_per_usdt REAL NOT NULL DEFAULT 1,
  total_idr REAL NOT NULL DEFAULT 0,
  total_usdt REAL NOT NULL DEFAULT 0,
  usdt_address TEXT NOT NULL DEFAULT '',
  usdt_network TEXT NOT NULL DEFAULT '',
  rate_source TEXT NOT NULL DEFAULT '',
  payment_instructions TEXT NOT NULL DEFAULT '',
  hash_version INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_invoices_key ON invoices(key_id, to_seq DESC);
CREATE INDEX IF NOT EXISTS idx_invoices_issued ON invoices(issued_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_invoices_number ON invoices(number);

-- An invoice's figures are as fixed as the ledger rows they were drawn from.
-- Only the two fields that describe what has happened to the invoice since —
-- whether it was paid or voided, and when — may be written again.
CREATE TRIGGER IF NOT EXISTS invoices_amounts_are_final
BEFORE UPDATE OF
  id, number, key_id, key_kind, issued_at, period_start, period_end,
  from_seq, to_seq, currency, requests, input_tokens, output_tokens,
  cached_tokens, reasoning_tokens, subtotal_usd, tax_percent, tax_usd,
  total_usd, backend_usd, lines, content_hash,
  due_at, issuer, idr_per_usdt, usd_per_usdt, total_idr, total_usdt,
  usdt_address, usdt_network, rate_source, hash_version
ON invoices
BEGIN
  SELECT RAISE(ABORT, 'an issued invoice cannot be restated: void it and issue another');
END;
";

/// Columns added after the table first shipped.
///
/// `CREATE TABLE IF NOT EXISTS` does nothing to a table that already exists, so
/// a database written by an earlier build still has that build's columns. These
/// are added by [`crate::store::migrate`] on start-up, with the defaults that
/// make an invoice issued before this change still read correctly: no due date,
/// no rate, no wallet, and `hash_version` 1 — the canonical form its content
/// hash was actually computed over.
pub const ADDED_COLUMNS: [(&str, &str); 11] = [
    ("due_at", "INTEGER NOT NULL DEFAULT 0"),
    ("issuer", "TEXT NOT NULL DEFAULT ''"),
    ("idr_per_usdt", "REAL NOT NULL DEFAULT 0"),
    ("usd_per_usdt", "REAL NOT NULL DEFAULT 1"),
    ("total_idr", "REAL NOT NULL DEFAULT 0"),
    ("total_usdt", "REAL NOT NULL DEFAULT 0"),
    ("usdt_address", "TEXT NOT NULL DEFAULT ''"),
    ("usdt_network", "TEXT NOT NULL DEFAULT ''"),
    ("rate_source", "TEXT NOT NULL DEFAULT ''"),
    ("payment_instructions", "TEXT NOT NULL DEFAULT ''"),
    ("hash_version", "INTEGER NOT NULL DEFAULT 1"),
];

/// The canonical form that covers the payment instruction as well as the
/// figures. Everything issued from this build on.
pub const HASH_V2: i64 = 2;

/// The statuses an invoice can be in.
pub const ISSUED: &str = "issued";
pub const PAID: &str = "paid";
pub const VOID: &str = "void";

/// One model's share of an invoice.
///
/// This is the "which models, and what each of them cost" line of the bill.
/// Tokens and money both, because a customer querying an invoice asks about
/// tokens and the invoice has to answer in the same units it charged in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvoiceLine {
    pub model: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    /// What this model came to, in USD, at the rates that applied when each of
    /// its requests ran — not at today's rate card. The ledger recorded the
    /// price with the request, so a rate change never rewrites an old bill.
    pub amount_usd: f64,
    /// What the relay paid its backend for the same traffic. Never shown to the
    /// customer; it is here so the operator can read margin per model off the
    /// invoice they just issued.
    pub backend_usd: f64,
}

/// What a key has run up, either since its last invoice or over all time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePeriod {
    /// Exclusive lower bound: the `to_seq` of the last invoice, or 0.
    pub from_seq: i64,
    /// Inclusive upper bound: the newest ledger row at the time of the read.
    pub to_seq: i64,
    /// When the period opened — the last invoice's `period_end`, or the first
    /// row in it.
    pub start_ts: i64,
    pub end_ts: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub billed_input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    /// The number the customer owes before tax.
    pub subtotal_usd: f64,
    pub backend_usd: f64,
    /// `subtotal_usd - backend_usd`.
    pub profit_usd: f64,
    /// How many distinct people were behind the key. Always 1 for a private
    /// key, which is the whole point of it being private.
    pub users: i64,
    /// Per model, biggest bill first.
    pub lines: Vec<InvoiceLine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invoice {
    pub id: String,
    pub number: String,
    pub key_id: String,
    pub key_label: String,
    pub key_kind: String,
    /// Name, email, address and tax id as they stood when this was issued,
    /// as JSON. Copied rather than referenced: an invoice has to still say who
    /// it was addressed to after the key has been renamed or deleted.
    pub bill_to: serde_json::Value,
    pub issued_at: i64,
    /// The last day the customer may pay without the invoice being late:
    /// `issued_at` plus `billing.dueDays`, frozen here so changing the terms
    /// never moves a deadline that has already been handed out. 0 on invoices
    /// issued before there were terms.
    pub due_at: i64,
    /// Who the invoice is from, as it stood when it was issued.
    ///
    /// Copied rather than read live, for the same reason `bill_to` is: an
    /// invoice is a statement about a moment. `Null` on invoices issued before
    /// this was recorded, and those fall back to the live issuer — which is
    /// what they have always done.
    pub issuer: serde_json::Value,
    pub period_start: i64,
    pub period_end: i64,
    pub from_seq: i64,
    pub to_seq: i64,
    pub currency: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    pub subtotal_usd: f64,
    pub tax_percent: f64,
    pub tax_usd: f64,
    pub total_usd: f64,
    pub backend_usd: f64,
    /// What one USDT was worth in rupiah on the day this was issued, and what
    /// it was worth in US dollars. Both frozen: the relay prices in USD, and
    /// these two are what turn that into the number the customer transfers.
    pub idr_per_usdt: f64,
    pub usd_per_usdt: f64,
    /// `total_usd / usd_per_usdt`, rounded to the six decimals a USDT transfer
    /// can actually carry.
    pub total_usdt: f64,
    /// `total_usdt * idr_per_usdt`, rounded to whole rupiah — there is no
    /// smaller unit in circulation.
    pub total_idr: f64,
    /// The wallet this invoice asked for, and the chain it is on. Frozen
    /// because a rotated wallet must not silently redirect an invoice somebody
    /// is still holding, and because sending USDT over the wrong chain
    /// destroys it.
    pub usdt_address: String,
    pub usdt_network: String,
    /// Where the rate came from, printed beside it.
    pub rate_source: String,
    pub payment_instructions: String,
    pub lines: Vec<InvoiceLine>,
    pub note: String,
    /// `issued`, `paid` or `void`.
    pub status: String,
    /// When it was paid or voided. 0 while it is neither.
    pub settled_at: i64,
    /// SHA-256 over everything above except `status`, `settled_at` and `note`.
    ///
    /// Those three are the only parts of an invoice that are allowed to change
    /// after it is issued, so they are the only parts the hash leaves out. The
    /// figures are covered, and SQLite refuses to write them again anyway; the
    /// hash is what catches an edit made around SQLite.
    pub content_hash: String,
    /// Which canonical form [`content_hash`](Self::content_hash) was computed
    /// over.
    ///
    /// Version 1 predates the payment and due-date fields. Adding those to the
    /// canonical form unconditionally would have made every invoice already on
    /// disk fail verification — reporting tampering where there was none, which
    /// is worse than no check at all, because an integrity check nobody
    /// believes is one nobody reads. So the form is versioned and each invoice
    /// is verified against the one it was actually written with.
    pub hash_version: i64,
}

impl Invoice {
    /// The bytes the content hash covers.
    fn canonical(&self) -> String {
        let lines = self
            .lines
            .iter()
            .map(|l| {
                format!(
                    "{}:{}:{}:{}:{}:{}:{:.9}:{:.9}",
                    l.model,
                    l.requests,
                    l.input_tokens,
                    l.output_tokens,
                    l.cached_tokens,
                    l.reasoning_tokens,
                    l.amount_usd,
                    l.backend_usd,
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let base = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{:.9}|{:.4}|{:.9}|{:.9}|{:.9}|[{lines}]",
            self.id,
            self.number,
            self.key_id,
            self.key_kind,
            self.issued_at,
            self.period_start,
            self.period_end,
            self.from_seq,
            self.to_seq,
            self.currency,
            self.requests,
            self.input_tokens,
            self.output_tokens,
            self.cached_tokens,
            self.reasoning_tokens,
            self.subtotal_usd,
            self.tax_percent,
            self.tax_usd,
            self.total_usd,
            self.backend_usd,
        );
        if self.hash_version < HASH_V2 {
            // Exactly the bytes an invoice written before the payment fields
            // existed was hashed over. Reproduced rather than approximated:
            // this is what makes those invoices still verify.
            return base;
        }
        // What the customer was told to pay, where, and at what rate. These
        // belong under the hash for the same reason the total does — they are
        // the instruction, and an instruction that can be edited after the
        // fact is not one.
        format!(
            "{base}|{}|{:.6}|{:.9}|{:.6}|{:.2}|{}|{}|{}",
            self.due_at,
            self.idr_per_usdt,
            self.usd_per_usdt,
            self.total_usdt,
            self.total_idr,
            self.usdt_address,
            self.usdt_network,
            self.rate_source,
        )
    }

    fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical().as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

/* ------------------------------------------------------------- reading -- */

/// Where this key's current period starts: the `to_seq` of its newest invoice
/// that has not been voided, or 0 if it has never been billed.
///
/// A voided invoice deliberately does not count, which is what hands its period
/// back: the next invoice reaches past it to the one before, and bills both
/// periods together.
pub fn billing_cursor(conn: &Connection, key_id: &str) -> rusqlite::Result<(i64, i64)> {
    conn.query_row(
        "SELECT to_seq, period_end FROM invoices
         WHERE key_id = ?1 AND status != 'void'
         ORDER BY to_seq DESC LIMIT 1",
        [key_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => Ok((0, 0)),
        other => Err(other),
    })
}

/// What a key has used over a range of ledger rows.
///
/// `from_seq` is exclusive and `to_seq` inclusive, so the range an invoice
/// covers and the range that comes after it never overlap by a row.
pub fn usage_between(
    conn: &Connection,
    key_id: &str,
    from_seq: i64,
    to_seq: i64,
) -> rusqlite::Result<UsagePeriod> {
    let mut period: UsagePeriod = conn.query_row(
        "SELECT COALESCE(SUM(requests),0), COALESCE(SUM(input_tokens),0),
                COALESCE(SUM(billed_input_tokens),0), COALESCE(SUM(output_tokens),0),
                COALESCE(SUM(cached_tokens),0), COALESCE(SUM(reasoning_tokens),0),
                COALESCE(SUM(proxy_usd),0), COALESCE(SUM(backend_usd),0),
                COUNT(DISTINCT NULLIF(user_id,'')),
                COALESCE(MIN(ts),0), COALESCE(MAX(ts),0)
         FROM usage_ledger WHERE key_id = ?1 AND seq > ?2 AND seq <= ?3",
        rusqlite::params![key_id, from_seq, to_seq],
        |r| {
            let subtotal: f64 = r.get(6)?;
            let backend: f64 = r.get(7)?;
            Ok(UsagePeriod {
                from_seq,
                to_seq,
                start_ts: r.get(9)?,
                end_ts: r.get(10)?,
                requests: r.get(0)?,
                input_tokens: r.get(1)?,
                billed_input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                cached_tokens: r.get(4)?,
                reasoning_tokens: r.get(5)?,
                subtotal_usd: round(subtotal, 9),
                backend_usd: round(backend, 9),
                profit_usd: round(subtotal - backend, 9),
                users: r.get(8)?,
                lines: Vec::new(),
            })
        },
    )?;

    let mut stmt = conn.prepare(
        "SELECT public_model, COALESCE(SUM(requests),0), COALESCE(SUM(input_tokens),0),
                COALESCE(SUM(output_tokens),0), COALESCE(SUM(cached_tokens),0),
                COALESCE(SUM(reasoning_tokens),0), COALESCE(SUM(proxy_usd),0),
                COALESCE(SUM(backend_usd),0)
         FROM usage_ledger
         WHERE key_id = ?1 AND seq > ?2 AND seq <= ?3 AND public_model != ''
         GROUP BY public_model
         ORDER BY SUM(proxy_usd) DESC, SUM(output_tokens) DESC",
    )?;
    period.lines = stmt
        .query_map(rusqlite::params![key_id, from_seq, to_seq], |r| {
            Ok(InvoiceLine {
                model: r.get(0)?,
                requests: r.get(1)?,
                input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                cached_tokens: r.get(4)?,
                reasoning_tokens: r.get(5)?,
                amount_usd: round(r.get::<_, f64>(6)?, 9),
                backend_usd: round(r.get::<_, f64>(7)?, 9),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(period)
}

/// The newest ledger row that exists right now, for any key.
fn head_seq(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM usage_ledger", [], |r| {
        r.get(0)
    })
}

/// This key's uninvoiced usage — what "current usage" means everywhere else.
pub fn current_usage(conn: &Connection, key_id: &str) -> rusqlite::Result<UsagePeriod> {
    let (from_seq, period_start) = billing_cursor(conn, key_id)?;
    let mut period = usage_between(conn, key_id, from_seq, head_seq(conn)?)?;
    if period_start > 0 {
        period.start_ts = period_start;
    }
    Ok(period)
}

/// Everything the key has ever run, invoiced or not.
pub fn lifetime_usage(conn: &Connection, key_id: &str) -> rusqlite::Result<UsagePeriod> {
    usage_between(conn, key_id, 0, i64::MAX)
}

/* ------------------------------------------------------------- issuing -- */

/// The next invoice number, as `PREFIX-YYYY-NNNN`.
///
/// The counter runs per year and per prefix, and is derived from what is
/// already in the table rather than stored, so it cannot drift out of step with
/// the invoices it is numbering.
fn next_number(conn: &Connection, prefix: &str, year: i32) -> rusqlite::Result<String> {
    let prefix = if prefix.trim().is_empty() {
        "INV"
    } else {
        prefix.trim()
    };
    let stem = format!("{prefix}-{year}-");
    let used: i64 = conn.query_row(
        "SELECT COUNT(*) FROM invoices WHERE number LIKE ?1 || '%'",
        [&stem],
        |r| r.get(0),
    )?;
    // Counting is enough in the ordinary case; the loop is what makes it
    // correct after an invoice has been deleted by hand out of the middle.
    for n in (used + 1)..(used + 1000) {
        let candidate = format!("{stem}{n:04}");
        let taken: i64 = conn.query_row(
            "SELECT COUNT(*) FROM invoices WHERE number = ?1",
            [&candidate],
            |r| r.get(0),
        )?;
        if taken == 0 {
            return Ok(candidate);
        }
    }
    Ok(format!("{stem}{}", crate::util::new_id("")))
}

/// What the caller asked for when issuing.
pub struct IssueRequest<'a> {
    pub key: &'a ClientKey,
    pub billing: &'a BillingConfig,
    pub note: String,
    /// Overrides both the key's rate and the global one. Used by the dashboard
    /// when the operator types a different number into the issue dialog.
    pub tax_percent: Option<f64>,
    /// Issue even when the total is under `billing.minimumUsd`, or when there
    /// is nothing to bill at all. The manual button sets this; the automatic
    /// cycle does not.
    pub force: bool,
    /// The relay's own zone, for the year an invoice number counts within.
    pub tz: chrono_tz::Tz,
}

/// Why an issue attempt produced no invoice. Neither is an error: an empty
/// period is the normal state of a key nobody has called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// No uninvoiced rows at all.
    NothingToBill,
    /// There is usage, but it comes to less than `billing.minimumUsd`.
    BelowMinimum,
}

pub enum Issued {
    Invoice(Box<Invoice>),
    Skipped(Skipped),
}

/// Draw the line: bill everything this key has run since its last invoice, and
/// start the next period.
///
/// Runs in one transaction, so the invoice either exists with the whole period
/// on it or does not exist at all — there is no state where a period has been
/// closed by an invoice that failed to write.
pub fn issue(conn: &mut Connection, req: IssueRequest<'_>) -> Result<Issued> {
    // Immediate, not deferred. A deferred transaction takes a read lock first
    // and upgrades on the INSERT — and an upgrade that finds the relay's own
    // writer holding the write lock fails with SQLITE_BUSY straight away
    // instead of waiting out `busy_timeout`, because waiting could deadlock.
    // Taking the write lock up front is what makes the timeout apply.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let key = req.key;

    let (from_seq, last_end) = billing_cursor(&tx, &key.id)?;
    // The line is drawn at the newest row that exists now. Anything the writer
    // commits after this belongs to the next period, which is both correct and
    // the reason issuing does not have to stop the world.
    let to_seq = head_seq(&tx)?;
    if to_seq <= from_seq {
        return Ok(Issued::Skipped(Skipped::NothingToBill));
    }

    let period = usage_between(&tx, &key.id, from_seq, to_seq)?;
    if period.requests == 0 && period.subtotal_usd <= 0.0 && !req.force {
        return Ok(Issued::Skipped(Skipped::NothingToBill));
    }

    let tax_percent = req
        .tax_percent
        .or(key.billing.tax_percent)
        .unwrap_or(req.billing.tax_percent)
        .max(0.0);
    let subtotal = round(period.subtotal_usd.max(0.0), 9);
    if !req.force && req.billing.minimum_usd > 0.0 && subtotal < req.billing.minimum_usd {
        return Ok(Issued::Skipped(Skipped::BelowMinimum));
    }

    let tax = round(subtotal * tax_percent / 100.0, 9);
    let total = round(subtotal + tax, 9);
    let now = crate::util::now_ms();
    let year = crate::util::local_parts(now, &req.tz).year;

    // The payment instruction, frozen. Read once here and copied onto the
    // invoice, never read live afterwards: the customer is being told an
    // address, a chain and a rate, and all three can change tomorrow.
    let pay = &req.billing.payment;
    // Guarded rather than trusted even though `normalize` already clamps it:
    // this divides, and a zero reaching it would put an infinity on a bill.
    let usd_per_usdt = if pay.usd_per_usdt.is_finite() && pay.usd_per_usdt > 0.0 {
        pay.usd_per_usdt
    } else {
        1.0
    };
    let idr_per_usdt = if pay.idr_per_usdt.is_finite() && pay.idr_per_usdt > 0.0 {
        pay.idr_per_usdt
    } else {
        0.0
    };
    // Six decimals because that is the smallest unit a USDT transfer carries,
    // and whole rupiah because there is no smaller one in circulation.
    let total_usdt = round(total / usd_per_usdt, 6);
    let total_idr = round(total_usdt * idr_per_usdt, 2);

    let mut invoice = Invoice {
        id: crate::util::new_id("inv"),
        number: next_number(&tx, &req.billing.number_prefix, year)?,
        key_id: key.id.clone(),
        key_label: key.display_name().to_string(),
        key_kind: key.kind.as_str().to_string(),
        bill_to: serde_json::json!({
            // The company is the name at the top of the invoice; `name` is the
            // person it goes to. Blank company means the two are one, and only
            // `name` is shown.
            "company": key.billing.company,
            "name": if key.billing.name.is_empty() { key.display_name() } else { &key.billing.name },
            "email": key.billing.email,
            "address": key.billing.address,
            "taxId": key.billing.tax_id,
        }),
        issued_at: now,
        // Three days by default, and whatever the operator set otherwise.
        // Frozen: shortening the terms next month must not retroactively make
        // an invoice somebody is holding overdue.
        due_at: now + i64::from(req.billing.due_days) * 86_400_000,
        issuer: serde_json::to_value(&req.billing.issuer).unwrap_or(serde_json::Value::Null),
        // A period runs from where the last one ended, so there is no gap
        // between two invoices even if the key was idle in between.
        period_start: if last_end > 0 {
            last_end
        } else {
            period.start_ts
        },
        period_end: now,
        from_seq,
        to_seq,
        currency: if req.billing.currency.trim().is_empty() {
            "USD".into()
        } else {
            req.billing.currency.trim().to_string()
        },
        requests: period.requests,
        input_tokens: period.input_tokens,
        output_tokens: period.output_tokens,
        cached_tokens: period.cached_tokens,
        reasoning_tokens: period.reasoning_tokens,
        subtotal_usd: subtotal,
        tax_percent,
        tax_usd: tax,
        total_usd: total,
        backend_usd: period.backend_usd,
        idr_per_usdt,
        usd_per_usdt,
        total_usdt,
        total_idr,
        usdt_address: pay.usdt_address.clone(),
        usdt_network: pay.usdt_network.clone(),
        rate_source: pay.rate_source.clone(),
        payment_instructions: pay.instructions.clone(),
        lines: period.lines,
        note: req.note,
        status: ISSUED.into(),
        settled_at: 0,
        content_hash: String::new(),
        hash_version: HASH_V2,
    };
    invoice.content_hash = invoice.hash();

    tx.execute(
        "INSERT INTO invoices (
           id, number, key_id, key_label, key_kind, bill_to, issued_at,
           period_start, period_end, from_seq, to_seq, currency, requests,
           input_tokens, output_tokens, cached_tokens, reasoning_tokens,
           subtotal_usd, tax_percent, tax_usd, total_usd, backend_usd,
           lines, note, status, settled_at, content_hash,
           due_at, issuer, idr_per_usdt, usd_per_usdt, total_idr, total_usdt,
           usdt_address, usdt_network, rate_source, payment_instructions,
           hash_version
         ) VALUES (
           ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
           ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27,
           ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38
         )",
        rusqlite::params![
            invoice.id,
            invoice.number,
            invoice.key_id,
            invoice.key_label,
            invoice.key_kind,
            invoice.bill_to.to_string(),
            invoice.issued_at,
            invoice.period_start,
            invoice.period_end,
            invoice.from_seq,
            invoice.to_seq,
            invoice.currency,
            invoice.requests,
            invoice.input_tokens,
            invoice.output_tokens,
            invoice.cached_tokens,
            invoice.reasoning_tokens,
            invoice.subtotal_usd,
            invoice.tax_percent,
            invoice.tax_usd,
            invoice.total_usd,
            invoice.backend_usd,
            serde_json::to_string(&invoice.lines)?,
            invoice.note,
            invoice.status,
            invoice.settled_at,
            invoice.content_hash,
            invoice.due_at,
            invoice.issuer.to_string(),
            invoice.idr_per_usdt,
            invoice.usd_per_usdt,
            invoice.total_idr,
            invoice.total_usdt,
            invoice.usdt_address,
            invoice.usdt_network,
            invoice.rate_source,
            invoice.payment_instructions,
            invoice.hash_version,
        ],
    )?;
    tx.commit()?;
    Ok(Issued::Invoice(Box::new(invoice)))
}

/// Mark an invoice paid or void.
///
/// The only write an issued invoice accepts, and SQLite enforces that: a
/// trigger refuses an update that touches any of the figures.
pub fn set_status(conn: &Connection, id: &str, status: &str) -> Result<Invoice> {
    let status = match status.trim().to_lowercase().as_str() {
        PAID => PAID,
        VOID => VOID,
        ISSUED => ISSUED,
        other => bail!("\"{other}\" is not an invoice status: paid, void or issued"),
    };

    let current = get(conn, id)?.ok_or_else(|| anyhow::anyhow!("no invoice with id \"{id}\""))?;
    if status == VOID && current.status != VOID {
        refuse_a_void_that_would_orphan_a_period(conn, &current)?;
    }

    let settled_at = if status == ISSUED {
        0
    } else {
        crate::util::now_ms()
    };
    conn.execute(
        "UPDATE invoices SET status = ?2, settled_at = ?3 WHERE id = ?1",
        rusqlite::params![id, status, settled_at],
    )?;
    get(conn, id)?.ok_or_else(|| anyhow::anyhow!("invoice \"{id}\" vanished while being updated"))
}

/// Only the newest invoice may be voided, and this is why.
///
/// A period is a range of `seq`, and where the next one starts is read off the
/// newest invoice that still stands. Void one from the middle of the run and
/// its range belongs to nothing: the cursor still sits at the newest invoice
/// past it, so those rows are never billed to anyone and never appear as
/// unbilled either. The money does not come back — it disappears, quietly,
/// which is the worst way for money to behave.
///
/// The alternative to refusing is tracking a set of ranges with holes in it,
/// per key, and asking every read to work out what is not covered. At the
/// volumes this relay is built for — millions of ledger rows in a billing
/// period — that turns the cheapest query in the billing screen into a scan of
/// the whole history. Voiding out of order is rare; losing a period is not
/// something to risk to make it convenient.
///
/// So: void the newest, then the one before it, and re-issue. That is also
/// what an accountant would do, and it leaves the run of periods unbroken.
fn refuse_a_void_that_would_orphan_a_period(conn: &Connection, invoice: &Invoice) -> Result<()> {
    let (newest_seq, _) = billing_cursor(conn, &invoice.key_id)?;
    if invoice.to_seq == newest_seq {
        return Ok(());
    }
    let newer: Vec<String> = conn
        .prepare(
            "SELECT number FROM invoices
             WHERE key_id = ?1 AND status != 'void' AND to_seq > ?2
             ORDER BY to_seq DESC",
        )?
        .query_map(rusqlite::params![invoice.key_id, invoice.to_seq], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    bail!(
        "{} is not the newest invoice for this key, and voiding it would leave the usage it \
         covers billed to nothing. Void {} first — newest first — then this one.",
        invoice.number,
        newer.join(", then "),
    )
}

const SELECT_SQL: &str = "SELECT id, number, key_id, key_label, key_kind, bill_to, issued_at,
       period_start, period_end, from_seq, to_seq, currency, requests,
       input_tokens, output_tokens, cached_tokens, reasoning_tokens,
       subtotal_usd, tax_percent, tax_usd, total_usd, backend_usd,
       lines, note, status, settled_at, content_hash,
       due_at, issuer, idr_per_usdt, usd_per_usdt, total_idr, total_usdt,
       usdt_address, usdt_network, rate_source, payment_instructions,
       hash_version
FROM invoices";

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Invoice> {
    Ok(Invoice {
        id: row.get(0)?,
        number: row.get(1)?,
        key_id: row.get(2)?,
        key_label: row.get(3)?,
        key_kind: row.get(4)?,
        bill_to: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null),
        issued_at: row.get(6)?,
        period_start: row.get(7)?,
        period_end: row.get(8)?,
        from_seq: row.get(9)?,
        to_seq: row.get(10)?,
        currency: row.get(11)?,
        requests: row.get(12)?,
        input_tokens: row.get(13)?,
        output_tokens: row.get(14)?,
        cached_tokens: row.get(15)?,
        reasoning_tokens: row.get(16)?,
        subtotal_usd: row.get(17)?,
        tax_percent: row.get(18)?,
        tax_usd: row.get(19)?,
        total_usd: row.get(20)?,
        backend_usd: row.get(21)?,
        lines: serde_json::from_str(&row.get::<_, String>(22)?).unwrap_or_default(),
        note: row.get(23)?,
        status: row.get(24)?,
        settled_at: row.get(25)?,
        content_hash: row.get(26)?,
        due_at: row.get(27)?,
        issuer: serde_json::from_str(&row.get::<_, String>(28)?).unwrap_or(serde_json::Value::Null),
        idr_per_usdt: row.get(29)?,
        usd_per_usdt: row.get(30)?,
        total_idr: row.get(31)?,
        total_usdt: row.get(32)?,
        usdt_address: row.get(33)?,
        usdt_network: row.get(34)?,
        rate_source: row.get(35)?,
        payment_instructions: row.get(36)?,
        hash_version: row.get(37)?,
    })
}

pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<Invoice>> {
    let mut stmt = conn.prepare(&format!("{SELECT_SQL} WHERE id = ?1"))?;
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(row) => Ok(Some(read_row(row)?)),
        None => Ok(None),
    }
}

/// Recent invoices, newest first, optionally for one key.
pub fn list(conn: &Connection, key_id: Option<&str>, limit: i64) -> rusqlite::Result<Vec<Invoice>> {
    let (sql, params): (String, Vec<rusqlite::types::Value>) = match key_id {
        Some(id) => (
            format!("{SELECT_SQL} WHERE key_id = ?1 ORDER BY issued_at DESC LIMIT ?2"),
            vec![id.to_string().into(), limit.into()],
        ),
        None => (
            format!("{SELECT_SQL} ORDER BY issued_at DESC LIMIT ?1"),
            vec![limit.into()],
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), read_row)?
        .collect::<rusqlite::Result<Vec<_>>>();
    rows
}

/// Recompute every invoice's content hash and report the first that disagrees.
pub fn verify(conn: &Connection) -> rusqlite::Result<Verification> {
    let mut stmt = conn.prepare(&format!("{SELECT_SQL} ORDER BY issued_at ASC"))?;
    let mut rows = stmt.query([])?;
    let mut count = 0i64;
    while let Some(row) = rows.next()? {
        let invoice = read_row(row)?;
        count += 1;
        if invoice.hash() != invoice.content_hash {
            return Ok(Verification {
                ok: false,
                invoices: count,
                broken: Some(invoice.number.clone()),
                message: format!(
                    "invoice {} does not match its own figures: it was edited after it was issued",
                    invoice.number
                ),
            });
        }
    }
    Ok(Verification {
        ok: true,
        invoices: count,
        broken: None,
        message: format!("all {count} invoices match their figures"),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verification {
    pub ok: bool,
    pub invoices: i64,
    /// The number of the first invoice that does not match, if any.
    pub broken: Option<String>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeyKind;
    use crate::store::ledger::{self, LedgerEntry, Phase};

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(ledger::CREATE_TABLE_SQL).unwrap();
        conn.execute_batch(ledger::INDEX_SQL).unwrap();
        conn.execute_batch(CREATE_SQL).unwrap();
        conn
    }

    /// Append one priced row for a key, the way the relay's writer would.
    fn spend(conn: &Connection, key_id: &str, model: &str, usd: f64) {
        let entry = LedgerEntry {
            request_id: crate::util::new_id("req"),
            phase: Phase::Final,
            ts: crate::util::now_ms(),
            day: "2026-09-13".into(),
            key_id: key_id.into(),
            public_model: model.into(),
            status: 200,
            requests: 1,
            input_tokens: 100,
            output_tokens: 50,
            proxy_usd: usd,
            backend_usd: usd / 2.0,
            user_id: "u_one".into(),
            key_kind: KeyKind::Private.as_str().into(),
            ..Default::default()
        };
        let hash = entry.hash_with(ledger::GENESIS);
        conn.execute(
            ledger::INSERT_SQL,
            rusqlite::params_from_iter(entry.as_params(ledger::GENESIS, &hash)),
        )
        .unwrap();
    }

    fn key() -> ClientKey {
        ClientKey {
            id: "key_1".into(),
            label: "acme".into(),
            key: "Kunci-Zeiko-test".into(),
            kind: KeyKind::Company,
            ..Default::default()
        }
    }

    fn request<'a>(key: &'a ClientKey, billing: &'a BillingConfig) -> IssueRequest<'a> {
        IssueRequest {
            key,
            billing,
            note: String::new(),
            tax_percent: None,
            force: false,
            tz: chrono_tz::UTC,
        }
    }

    /// Billing that actually says where the money goes: a company, a wallet, a
    /// chain, and a rate.
    fn paying() -> BillingConfig {
        BillingConfig {
            due_days: 3,
            issuer: crate::config::Issuer {
                name: "PT Zeiko Relay Indonesia".into(),
                email: "billing@zeiko.id".into(),
                address: "Jakarta Selatan".into(),
                tax_id: "01.234.567.8-901.000".into(),
                payment_terms: "USDT only".into(),
            },
            payment: crate::config::PaymentConfig {
                usdt_address: "TQn9Y2khEsLJW1ChVWFMSMeRDow5KcbLSE".into(),
                usdt_network: "TRC20".into(),
                idr_per_usdt: 16_250.0,
                usd_per_usdt: 1.0,
                rate_source: "Indodax mid, 2026-09-16".into(),
                rate_updated_at: 1_760_000_000_000,
                instructions: "Put the invoice number in the memo.".into(),
            },
            ..BillingConfig::default()
        }
    }

    /// The deadline is three days after the invoice date, and it is *on* the
    /// invoice rather than recomputed from today's settings.
    #[test]
    fn the_payment_deadline_is_frozen_three_days_after_the_invoice_date() {
        let mut conn = db();
        let key = key();
        let billing = paying();
        spend(&conn, &key.id, "model-a", 4.0);

        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("expected an invoice");
        };
        assert_eq!(
            invoice.due_at - invoice.issued_at,
            3 * 86_400_000,
            "three days, in milliseconds"
        );

        // Shortening the terms afterwards must not move a deadline somebody is
        // already holding.
        let stricter = BillingConfig {
            due_days: 1,
            ..paying()
        };
        spend(&conn, &key.id, "model-a", 4.0);
        let Issued::Invoice(next) = issue(&mut conn, request(&key, &stricter)).unwrap() else {
            panic!("expected a second invoice");
        };
        assert_eq!(next.due_at - next.issued_at, 86_400_000);
        let reread = get(&conn, &invoice.id).unwrap().unwrap();
        assert_eq!(reread.due_at, invoice.due_at, "the first deadline moved");
    }

    /// What the customer is told to send, and where. All of it copied onto the
    /// invoice, because every one of these can change tomorrow.
    #[test]
    fn the_wallet_the_chain_and_the_rate_are_copied_onto_the_invoice() {
        let mut conn = db();
        let key = key();
        spend(&conn, &key.id, "model-a", 10.0);

        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &paying())).unwrap() else {
            panic!("expected an invoice");
        };
        assert_eq!(invoice.usdt_address, "TQn9Y2khEsLJW1ChVWFMSMeRDow5KcbLSE");
        assert_eq!(invoice.usdt_network, "TRC20");
        assert_eq!(invoice.idr_per_usdt, 16_250.0);
        assert_eq!(invoice.total_usd, 10.0);
        assert_eq!(invoice.total_usdt, 10.0);
        assert_eq!(invoice.total_idr, 162_500.0);
        assert_eq!(invoice.issuer["name"], "PT Zeiko Relay Indonesia");

        // Rotate the wallet and move the rate. The invoice already issued says
        // what it always said.
        let moved = BillingConfig {
            payment: crate::config::PaymentConfig {
                usdt_address: "0xdeadbeef".into(),
                usdt_network: "BEP20".into(),
                idr_per_usdt: 99_999.0,
                ..paying().payment
            },
            ..paying()
        };
        spend(&conn, &key.id, "model-a", 1.0);
        let _ = issue(&mut conn, request(&key, &moved)).unwrap();

        let reread = get(&conn, &invoice.id).unwrap().unwrap();
        assert_eq!(reread.usdt_address, "TQn9Y2khEsLJW1ChVWFMSMeRDow5KcbLSE");
        assert_eq!(reread.usdt_network, "TRC20");
        assert_eq!(reread.idr_per_usdt, 16_250.0);
        assert_eq!(reread.total_idr, 162_500.0);
    }

    /// USDT has broken its peg before. An invoice that assumed one dollar per
    /// USDT would misprice itself exactly on the day it mattered.
    #[test]
    fn the_usd_to_usdt_leg_is_applied_rather_than_assumed_to_be_one() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig {
            payment: crate::config::PaymentConfig {
                usd_per_usdt: 0.98,
                idr_per_usdt: 16_000.0,
                ..paying().payment
            },
            ..paying()
        };
        spend(&conn, &key.id, "model-a", 98.0);

        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("expected an invoice");
        };
        assert_eq!(invoice.total_usd, 98.0);
        assert_eq!(invoice.total_usdt, 100.0, "98 USD at 0.98 is 100 USDT");
        assert_eq!(invoice.total_idr, 1_600_000.0);
    }

    /// A rate of zero is a rate nobody has set yet. It must produce an empty
    /// figure rather than an infinity or a NaN on a bill.
    #[test]
    fn an_unset_rate_produces_no_rupiah_figure_rather_than_nonsense() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig {
            payment: crate::config::PaymentConfig {
                idr_per_usdt: 0.0,
                usd_per_usdt: 0.0,
                ..crate::config::PaymentConfig::default()
            },
            ..BillingConfig::default()
        };
        spend(&conn, &key.id, "model-a", 7.0);

        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("expected an invoice");
        };
        assert!(invoice.total_usdt.is_finite() && invoice.total_idr.is_finite());
        assert_eq!(invoice.total_usdt, 7.0, "a zero peg falls back to one");
        assert_eq!(invoice.total_idr, 0.0);
    }

    /// The tax line and the per-model lines are the detail a customer queries,
    /// and the rupiah total has to be the *taxed* total rather than the
    /// subtotal — a bill that converts the wrong number is worse than one that
    /// does not convert at all.
    #[test]
    fn the_rupiah_total_is_taken_from_the_taxed_total() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig {
            tax_percent: 11.0,
            ..paying()
        };
        spend(&conn, &key.id, "model-a", 100.0);

        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("expected an invoice");
        };
        assert_eq!(invoice.subtotal_usd, 100.0);
        assert_eq!(invoice.tax_usd, 11.0);
        assert_eq!(invoice.total_usd, 111.0);
        assert_eq!(invoice.total_usdt, 111.0);
        assert_eq!(invoice.total_idr, 111.0 * 16_250.0);
    }

    /// The payment instruction is part of what the invoice says, so it is
    /// under the hash: an address swapped around SQLite has to be as visible
    /// as a total swapped around SQLite.
    #[test]
    fn verification_notices_a_wallet_address_edited_around_sqlite() {
        let mut conn = db();
        let key = key();
        spend(&conn, &key.id, "model-a", 5.0);
        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &paying())).unwrap() else {
            panic!("expected an invoice");
        };
        assert!(verify(&conn).unwrap().ok);

        // The trigger refuses this, so go round it the way an attacker would.
        conn.execute_batch("DROP TRIGGER invoices_amounts_are_final")
            .unwrap();
        conn.execute(
            "UPDATE invoices SET usdt_address = ?2 WHERE id = ?1",
            rusqlite::params![invoice.id, "0xattacker"],
        )
        .unwrap();

        let checked = verify(&conn).unwrap();
        assert!(!checked.ok, "a redirected payment went unnoticed");
        assert_eq!(checked.broken.as_deref(), Some(invoice.number.as_str()));
    }

    /// SQLite refuses the edit outright, which is the first line of defence.
    #[test]
    fn the_payment_instruction_on_an_issued_invoice_cannot_be_rewritten() {
        let mut conn = db();
        let key = key();
        spend(&conn, &key.id, "model-a", 5.0);
        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &paying())).unwrap() else {
            panic!("expected an invoice");
        };

        for (column, value) in [
            ("usdt_address", "0xattacker"),
            ("usdt_network", "BEP20"),
            ("due_at", "0"),
        ] {
            let sql = format!("UPDATE invoices SET {column} = ?2 WHERE id = ?1");
            let err = conn
                .execute(&sql, rusqlite::params![invoice.id, value])
                .unwrap_err()
                .to_string();
            assert!(err.contains("cannot be restated"), "{column}: {err}");
        }
    }

    /// An invoice written before any of this existed still verifies.
    ///
    /// This is the whole reason the canonical form is versioned: hashing the
    /// new fields unconditionally would have reported every old invoice as
    /// tampered with, and an integrity check that cries wolf is one nobody
    /// reads.
    #[test]
    fn an_invoice_from_before_the_payment_fields_still_verifies() {
        let mut conn = db();
        let key = key();
        spend(&conn, &key.id, "model-a", 3.0);
        let Issued::Invoice(mut invoice) = issue(&mut conn, request(&key, &paying())).unwrap()
        else {
            panic!("expected an invoice");
        };

        // Rewrite it as an older build would have written it: version 1, no
        // payment fields, and the hash that form produces.
        invoice.hash_version = 1;
        invoice.due_at = 0;
        invoice.idr_per_usdt = 0.0;
        invoice.usd_per_usdt = 1.0;
        invoice.total_idr = 0.0;
        invoice.total_usdt = 0.0;
        invoice.usdt_address = String::new();
        invoice.usdt_network = String::new();
        invoice.rate_source = String::new();
        let legacy_hash = invoice.hash();

        conn.execute_batch("DROP TRIGGER invoices_amounts_are_final")
            .unwrap();
        conn.execute(
            "UPDATE invoices SET hash_version = 1, due_at = 0, idr_per_usdt = 0,
                    usd_per_usdt = 1, total_idr = 0, total_usdt = 0,
                    usdt_address = '', usdt_network = '', rate_source = '',
                    content_hash = ?2
             WHERE id = ?1",
            rusqlite::params![invoice.id, legacy_hash],
        )
        .unwrap();

        let checked = verify(&conn).unwrap();
        assert!(checked.ok, "{}", checked.message);
    }

    #[test]
    fn an_invoice_covers_the_usage_and_the_next_period_starts_empty() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig::default();

        spend(&conn, &key.id, "model-a", 1.0);
        spend(&conn, &key.id, "model-b", 0.5);
        spend(&conn, &key.id, "model-a", 0.25);

        let before = current_usage(&conn, &key.id).unwrap();
        assert_eq!(before.requests, 3);
        assert!((before.subtotal_usd - 1.75).abs() < 1e-9);

        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("three priced requests must produce an invoice");
        };
        assert_eq!(invoice.requests, 3);
        assert!((invoice.total_usd - 1.75).abs() < 1e-9);

        // The per-model breakdown, biggest bill first.
        assert_eq!(invoice.lines.len(), 2);
        assert_eq!(invoice.lines[0].model, "model-a");
        assert!((invoice.lines[0].amount_usd - 1.25).abs() < 1e-9);
        assert_eq!(invoice.lines[1].model, "model-b");

        // This is the reset: nothing was deleted, and current usage is zero.
        let after = current_usage(&conn, &key.id).unwrap();
        assert_eq!(after.requests, 0);
        assert_eq!(after.subtotal_usd, 0.0);

        // The history is still all there.
        let lifetime = lifetime_usage(&conn, &key.id).unwrap();
        assert_eq!(lifetime.requests, 3);

        // And a second invoice with nothing new to bill is not an invoice.
        assert!(matches!(
            issue(&mut conn, request(&key, &billing)).unwrap(),
            Issued::Skipped(Skipped::NothingToBill)
        ));

        // New traffic lands in the new period only.
        spend(&conn, &key.id, "model-a", 2.0);
        let Issued::Invoice(second) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("the new period has usage in it");
        };
        assert_eq!(second.requests, 1);
        assert!((second.subtotal_usd - 2.0).abs() < 1e-9);
        assert_eq!(
            second.from_seq, invoice.to_seq,
            "the periods must not overlap"
        );
        assert_ne!(second.number, invoice.number);
    }

    #[test]
    fn one_keys_invoice_leaves_another_keys_usage_alone() {
        let mut conn = db();
        let billing = BillingConfig::default();
        let mine = key();
        let theirs = ClientKey {
            id: "key_2".into(),
            ..key()
        };

        spend(&conn, &mine.id, "model-a", 1.0);
        spend(&conn, &theirs.id, "model-a", 3.0);

        let Issued::Invoice(invoice) = issue(&mut conn, request(&mine, &billing)).unwrap() else {
            panic!("there is usage to bill");
        };
        assert!((invoice.total_usd - 1.0).abs() < 1e-9);
        assert_eq!(current_usage(&conn, &theirs.id).unwrap().requests, 1);
    }

    #[test]
    fn tax_comes_from_the_key_before_the_global_rate() {
        let mut conn = db();
        let billing = BillingConfig {
            tax_percent: 10.0,
            ..BillingConfig::default()
        };
        let mut key = key();
        key.billing.tax_percent = Some(11.0);
        spend(&conn, &key.id, "model-a", 100.0);

        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("there is usage to bill");
        };
        assert!((invoice.tax_usd - 11.0).abs() < 1e-9);
        assert!((invoice.total_usd - 111.0).abs() < 1e-9);
    }

    #[test]
    fn a_voided_invoice_hands_its_period_to_the_next_one() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig::default();

        spend(&conn, &key.id, "model-a", 4.0);
        let Issued::Invoice(first) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("there is usage to bill");
        };
        set_status(&conn, &first.id, VOID).unwrap();

        // Voided, so the usage it covered is uninvoiced again.
        assert_eq!(current_usage(&conn, &key.id).unwrap().requests, 1);

        spend(&conn, &key.id, "model-a", 1.0);
        let Issued::Invoice(second) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("both periods are billable now");
        };
        assert_eq!(second.requests, 2);
        assert!((second.subtotal_usd - 5.0).abs() < 1e-9);
    }

    /// The bug this closes: with three periods billed and the middle invoice
    /// voided, the cursor still sat at the newest one — so the middle period
    /// was billed to nothing, appeared as unbilled to nobody, and its money
    /// simply left the books.
    #[test]
    fn a_middle_invoice_cannot_be_voided_because_its_period_would_vanish() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig::default();

        let mut issued = Vec::new();
        for usd in [1.0, 2.0, 4.0] {
            spend(&conn, &key.id, "model-a", usd);
            let Issued::Invoice(inv) = issue(&mut conn, request(&key, &billing)).unwrap() else {
                panic!("each period has usage in it");
            };
            issued.push(*inv);
        }

        let refused = set_status(&conn, &issued[1].id, VOID);
        let message = refused
            .expect_err("voiding the middle invoice must be refused")
            .to_string();
        assert!(
            message.contains(&issued[2].number),
            "the refusal should name what to void first, got: {message}"
        );

        // Nothing moved, and nothing was lost.
        assert_eq!(get(&conn, &issued[1].id).unwrap().unwrap().status, ISSUED);
        assert_eq!(current_usage(&conn, &key.id).unwrap().requests, 0);

        // Newest first is allowed, and hands each period back as it goes.
        set_status(&conn, &issued[2].id, VOID).unwrap();
        assert!((current_usage(&conn, &key.id).unwrap().subtotal_usd - 4.0).abs() < 1e-9);
        set_status(&conn, &issued[1].id, VOID).unwrap();
        let back = current_usage(&conn, &key.id).unwrap();
        assert_eq!(back.requests, 2);
        assert!(
            (back.subtotal_usd - 6.0).abs() < 1e-9,
            "both voided periods are billable again, not just the last one"
        );

        // And re-issuing covers exactly what came back.
        let Issued::Invoice(again) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("there is usage to bill");
        };
        assert_eq!(again.requests, 2);
        assert!((again.subtotal_usd - 6.0).abs() < 1e-9);
    }

    /// Every ledger row a key ever wrote belongs to exactly one standing
    /// invoice or to the open period — never to neither. This is the invariant
    /// the rule above exists to hold, checked over a run of issues and voids.
    #[test]
    fn no_usage_can_fall_between_two_periods() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig::default();

        let mut total = 0.0;
        let mut issued = Vec::new();
        for n in 1..=4 {
            spend(&conn, &key.id, "model-a", f64::from(n));
            total += f64::from(n);
            if let Issued::Invoice(inv) = issue(&mut conn, request(&key, &billing)).unwrap() {
                issued.push(*inv);
            }
        }

        // Whatever has been voided, billed plus unbilled is always everything.
        let accounted = |conn: &Connection| -> f64 {
            let billed: f64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(subtotal_usd),0) FROM invoices
                     WHERE key_id = ?1 AND status != 'void'",
                    [&key.id],
                    |r| r.get(0),
                )
                .unwrap();
            billed + current_usage(conn, &key.id).unwrap().subtotal_usd
        };

        assert!((accounted(&conn) - total).abs() < 1e-9);
        for invoice in issued.iter().rev() {
            set_status(&conn, &invoice.id, VOID).unwrap();
            assert!(
                (accounted(&conn) - total).abs() < 1e-9,
                "voiding {} lost money",
                invoice.number
            );
        }
        // Everything is unbilled again once every invoice is void.
        assert!((current_usage(&conn, &key.id).unwrap().subtotal_usd - total).abs() < 1e-9);
    }

    #[test]
    fn a_minimum_holds_the_period_open_rather_than_billing_small_change() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig {
            minimum_usd: 10.0,
            ..BillingConfig::default()
        };
        spend(&conn, &key.id, "model-a", 1.0);
        assert!(matches!(
            issue(&mut conn, request(&key, &billing)).unwrap(),
            Issued::Skipped(Skipped::BelowMinimum)
        ));
        // Held open, not thrown away.
        assert_eq!(current_usage(&conn, &key.id).unwrap().requests, 1);

        spend(&conn, &key.id, "model-a", 20.0);
        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("past the minimum now");
        };
        assert_eq!(invoice.requests, 2);
    }

    #[test]
    fn the_figures_on_an_issued_invoice_cannot_be_rewritten() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig::default();
        spend(&conn, &key.id, "model-a", 7.0);
        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("there is usage to bill");
        };

        let refused = conn.execute(
            "UPDATE invoices SET total_usd = 0 WHERE id = ?1",
            [&invoice.id],
        );
        assert!(refused.is_err(), "SQLite must refuse to restate a total");

        // Marking it paid is the one write that is allowed.
        let paid = set_status(&conn, &invoice.id, PAID).unwrap();
        assert_eq!(paid.status, PAID);
        assert!(paid.settled_at > 0);
        assert!(verify(&conn).unwrap().ok);
    }

    #[test]
    fn verification_notices_a_total_edited_around_sqlite() {
        let mut conn = db();
        let key = key();
        let billing = BillingConfig::default();
        spend(&conn, &key.id, "model-a", 7.0);
        let Issued::Invoice(invoice) = issue(&mut conn, request(&key, &billing)).unwrap() else {
            panic!("there is usage to bill");
        };
        assert!(verify(&conn).unwrap().ok);

        // Exactly what somebody with the file and the sqlite3 shell would do.
        conn.execute_batch("DROP TRIGGER invoices_amounts_are_final")
            .unwrap();
        conn.execute("UPDATE invoices SET total_usd = 0.01", [])
            .unwrap();

        let broken = verify(&conn).unwrap();
        assert!(!broken.ok);
        assert_eq!(broken.broken.as_deref(), Some(invoice.number.as_str()));
    }
}
