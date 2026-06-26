use metrics::counter;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(serde::Serialize)]
pub struct LedgerEntry {
    pub id: Uuid,
    pub event_type: String,
    pub listing_id: Uuid,
    pub mentor_id: Uuid,
    pub student_id: Uuid,
    pub gross_cents: i32,
    pub platform_fee_cents: i32,
    pub mentor_payout_cents: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub fn fee_breakdown(gross: i32, platform_fee_percent: i32) -> (i32, i32) {
    let fee = gross * platform_fee_percent / 100;
    let payout = gross - fee;
    (fee, payout)
}

pub async fn write_ledger(
    executor: impl sqlx::Executor<'_, Database = Postgres>,
    idempotency_key: &str,
    event_type: &str,
    listing_id: &str,
    mentor_id: &str,
    student_id: &str,
    gross: i32,
    fee: i32,
    payout: i32,
    payment_intent: Option<&str>,
    transfer_id: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO ledger_entries (idempotency_key, event_type, listing_id, mentor_id, student_id, gross_cents, platform_fee_cents, mentor_payout_cents, stripe_payment_intent_id, stripe_transfer_id)
         VALUES ($1,$2,$3::uuid,$4::uuid,$5::uuid,$6,$7,$8,$9,$10)
         ON CONFLICT (idempotency_key) DO NOTHING",
    )
    .bind(idempotency_key)
    .bind(event_type)
    .bind(listing_id)
    .bind(mentor_id)
    .bind(student_id)
    .bind(gross)
    .bind(fee)
    .bind(payout)
    .bind(payment_intent)
    .bind(transfer_id)
    .execute(executor)
    .await?;

    if result.rows_affected() > 0 {
        counter!("ledger_writes_total").increment(1);
        Ok(true)
    } else {
        Ok(false)
    }
}

pub async fn get_ledger_for_listing(
    pool: &PgPool,
    listing_id: &str,
) -> Result<Vec<LedgerEntry>, sqlx::Error> {
    sqlx::query_as::<_, LedgerEntry>(
        "SELECT id, event_type, listing_id, mentor_id, student_id, gross_cents, platform_fee_cents, mentor_payout_cents, created_at
         FROM ledger_entries WHERE listing_id = $1::uuid ORDER BY created_at DESC",
    )
    .bind(listing_id)
    .fetch_all(pool)
    .await
}

pub async fn payout_summary(pool: &PgPool) -> Result<(i64, i64, i64, i64), sqlx::Error> {
    let row: (Option<i64>, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT COALESCE(SUM(gross_cents),0), COALESCE(SUM(mentor_payout_cents),0), COALESCE(SUM(platform_fee_cents),0), COUNT(*)
         FROM ledger_entries",
    )
    .fetch_one(pool)
    .await?;
    Ok((
        row.0.unwrap_or(0),
        row.1.unwrap_or(0),
        row.2.unwrap_or(0),
        row.3.unwrap_or(0),
    ))
}

pub async fn can_view_ledger(
    general_pool: &PgPool,
    listing_id: &str,
    user_id: &str,
    role: &str,
) -> Result<bool, sqlx::Error> {
    if role == "admin" {
        return Ok(true);
    }

    let mentor_match: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM listings WHERE id = $1::uuid AND mentor_id = $2::uuid)",
    )
    .bind(listing_id)
    .bind(user_id)
    .fetch_one(general_pool)
    .await?;

    if mentor_match == Some(true) {
        return Ok(true);
    }

    let student_match: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM enrollments WHERE listing_id = $1::uuid AND student_id = $2::uuid)",
    )
    .bind(listing_id)
    .bind(user_id)
    .fetch_one(general_pool)
    .await?;

    Ok(student_match == Some(true))
}

impl sqlx::FromRow<'_, sqlx::postgres::PgRow> for LedgerEntry {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(LedgerEntry {
            id: row.try_get("id")?,
            event_type: row.try_get("event_type")?,
            listing_id: row.try_get("listing_id")?,
            mentor_id: row.try_get("mentor_id")?,
            student_id: row.try_get("student_id")?,
            gross_cents: row.try_get("gross_cents")?,
            platform_fee_cents: row.try_get("platform_fee_cents")?,
            mentor_payout_cents: row.try_get("mentor_payout_cents")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

pub async fn record_payout_event(
    tx: &mut Transaction<'_, Postgres>,
    event_id: &str,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO payout_events (stripe_event_id, event_type, raw_payload) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",
    )
    .bind(event_id)
    .bind(event_type)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn mark_payout_processed(
    tx: &mut Transaction<'_, Postgres>,
    event_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE payout_events SET processed = TRUE WHERE stripe_event_id = $1")
        .bind(event_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
