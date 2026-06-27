use crate::ledger::{
    can_view_ledger, fee_breakdown, get_ledger_for_listing, mark_payout_processed, payout_summary,
    record_payout_event, write_ledger,
};
use crate::outbox::enqueue_outbox;
use crate::sentry_util;
use crate::state::SharedState;
use crate::stripe::{create_account_link, create_checkout_session, create_express_account, verify_signature};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use metrics::counter;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct CheckoutRequest {
    pub listing_id: String,
    pub student_id: String,
    pub mentor_id: String,
    pub enrollment_id: String,
    pub amount_cents: i64,
    pub success_url: String,
    pub cancel_url: String,
}

#[derive(Serialize)]
pub struct CheckoutResponse {
    pub checkout_url: String,
    pub session_id: String,
}

#[derive(Serialize)]
pub struct PayoutSummary {
    pub total_gross_cents: i64,
    pub total_mentor_payout_cents: i64,
    pub total_platform_fee_cents: i64,
    pub transaction_count: i64,
}

#[derive(Deserialize)]
pub struct ConnectOnboardRequest {
    pub return_url: String,
    pub refresh_url: String,
}

#[derive(Serialize)]
pub struct ConnectOnboardResponse {
    pub onboarding_url: String,
    pub account_id: String,
}

#[derive(Serialize)]
pub struct ConnectStatusResponse {
    pub connected: bool,
    pub account_id: Option<String>,
}

pub async fn checkout(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<CheckoutRequest>,
) -> Result<Json<CheckoutResponse>, StatusCode> {
    let user_id = headers.get("X-User-Id").and_then(|v| v.to_str().ok());
    if user_id != Some(req.student_id.as_str()) {
        return Err(StatusCode::FORBIDDEN);
    }

    let gross = req.amount_cents as i32;
    let (fee, payout) = fee_breakdown(gross, state.platform_fee_percent);
    let session_id = format!("cs_test_{}", Uuid::new_v4());

    if !state.stripe_configured() {
        let mut tx = state.pool.begin().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        write_ledger(
            &mut *tx,
            &format!("checkout:{}", session_id),
            "checkout.completed",
            &req.listing_id,
            &req.mentor_id,
            &req.student_id,
            gross,
            fee,
            payout,
            Some(&session_id),
            None,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        enqueue_outbox(
            &mut *tx,
            "payout.recorded",
            &serde_json::json!({
                "enrollment_id": req.enrollment_id,
                "listing_id": req.listing_id,
                "gross_cents": gross,
                "mentor_payout_cents": payout,
            }),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        activate_enrollment(&state.users_url, &req.enrollment_id, &session_id).await;

        return Ok(Json(CheckoutResponse {
            checkout_url: req.success_url.clone(),
            session_id,
        }));
    }

    let mentor_account = mentor_stripe_account(&state, &req.mentor_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let body = create_checkout_session(
        &state.stripe_key,
        req.amount_cents,
        fee as i64,
        mentor_account.as_deref(),
        &req.success_url,
        &req.cancel_url,
        &req.listing_id,
        &req.student_id,
        &req.mentor_id,
        &req.enrollment_id,
    )
    .await
    .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if body.get("error").is_some() {
        tracing::error!("stripe checkout error: {:?}", body);
        return Err(StatusCode::BAD_GATEWAY);
    }

    Ok(Json(CheckoutResponse {
        checkout_url: body["url"]
            .as_str()
            .unwrap_or(&req.success_url)
            .to_string(),
        session_id: body["id"]
            .as_str()
            .unwrap_or(&session_id)
            .to_string(),
    }))
}

pub async fn stripe_webhook(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: String,
) -> Result<StatusCode, StatusCode> {
    if state.webhook_configured() {
        let sig = headers
            .get("stripe-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !verify_signature(&state.webhook_secret, &body, sig) {
            counter!("stripe_webhook_errors_total").increment(1);
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    let event: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            sentry_util::capture_error(&e, &[("handler", "stripe_webhook"), ("stage", "parse")]);
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    let event_id = event["id"].as_str().unwrap_or("unknown");
    let event_type = event["type"].as_str().unwrap_or("unknown");

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            sentry_util::capture_error(&e, &[("handler", "stripe_webhook"), ("stage", "begin_tx")]);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let is_new = match record_payout_event(&mut tx, event_id, event_type, &event).await {
        Ok(v) => v,
        Err(e) => {
            sentry_util::capture_error(&e, &[("handler", "stripe_webhook"), ("stage", "record_event")]);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    if !is_new {
        tx.commit().await.ok();
        return Ok(StatusCode::OK);
    }

    match event_type {
        "checkout.session.completed" => {
            if let Err(status) = process_checkout_completed(&state, &mut tx, &event, event_id).await {
                return Err(status);
            }
        }
        "account.updated" => {
            tracing::info!(
                "stripe account updated: {:?}",
                event["data"]["object"]["id"].as_str()
            );
        }
        _ => {}
    }

    if let Err(e) = mark_payout_processed(&mut tx, event_id).await {
        sentry_util::capture_error(&e, &[("handler", "stripe_webhook"), ("stage", "mark_processed")]);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    if let Err(e) = tx.commit().await {
        sentry_util::capture_error(&e, &[("handler", "stripe_webhook"), ("stage", "commit")]);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok(StatusCode::OK)
}

async fn process_checkout_completed(
    state: &SharedState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &serde_json::Value,
    event_id: &str,
) -> Result<(), StatusCode> {
    let obj = &event["data"]["object"];
    let session_id = obj["id"].as_str().unwrap_or("");
    let metadata = &obj["metadata"];
    let listing_id = metadata["listing_id"].as_str().unwrap_or("");
    let student_id = metadata["student_id"].as_str().unwrap_or("");
    let mentor_id = metadata["mentor_id"].as_str().unwrap_or("");
    let enrollment_id = metadata["enrollment_id"].as_str().unwrap_or("");
    let amount = obj["amount_total"].as_i64().unwrap_or(0) as i32;
    let (fee, payout) = fee_breakdown(amount, state.platform_fee_percent);
    let payment_intent = obj["payment_intent"].as_str();
    let transfer_id = obj["transfer"].as_str();

    let inserted = match write_ledger(
        &mut **tx,
        &format!("stripe:{}", event_id),
        "checkout.completed",
        listing_id,
        mentor_id,
        student_id,
        amount,
        fee,
        payout,
        payment_intent,
        transfer_id,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            sentry_util::capture_error(
                &e,
                &[("handler", "stripe_webhook"), ("stage", "write_ledger")],
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    if inserted {
        if let Err(e) = enqueue_outbox(
            &mut **tx,
            "payout.recorded",
            &serde_json::json!({
                "enrollment_id": enrollment_id,
                "listing_id": listing_id,
                "gross_cents": amount,
                "mentor_payout_cents": payout,
            }),
        )
        .await
        {
            sentry_util::capture_error(
                &e,
                &[("handler", "stripe_webhook"), ("stage", "enqueue_outbox")],
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }

        let users_url = state.users_url.clone();
        let enrollment_id = enrollment_id.to_string();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            activate_enrollment(&users_url, &enrollment_id, &session_id).await;
        });
    }

    Ok(())
}

pub async fn get_ledger(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(listing_id): Path<String>,
) -> Result<Json<Vec<crate::ledger::LedgerEntry>>, StatusCode> {
    let user_id = headers
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let role = headers
        .get("X-User-Role")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("student");

    let allowed = can_view_ledger(&state.general_pool, &listing_id, user_id, role)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !allowed {
        return Err(StatusCode::FORBIDDEN);
    }

    let rows = get_ledger_for_listing(&state.pool, &listing_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows))
}

pub async fn payout_summary_handler(
    State(state): State<SharedState>,
) -> Result<Json<PayoutSummary>, StatusCode> {
    let (gross, mentor, platform, count) = payout_summary(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(PayoutSummary {
        total_gross_cents: gross,
        total_mentor_payout_cents: mentor,
        total_platform_fee_cents: platform,
        transaction_count: count,
    }))
}

pub async fn connect_onboard(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ConnectOnboardRequest>,
) -> Result<Json<ConnectOnboardResponse>, StatusCode> {
    let user_id = headers
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let role = headers
        .get("X-User-Role")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if role != "mentor" && role != "admin" {
        return Err(StatusCode::FORBIDDEN);
    }

    if !state.stripe_configured() {
        return Ok(Json(ConnectOnboardResponse {
            onboarding_url: req.return_url,
            account_id: format!("acct_dev_{}", &user_id[..8.min(user_id.len())]),
        }));
    }

    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1::uuid")
        .bind(user_id)
        .fetch_one(&state.general_pool)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let account_id = match mentor_stripe_account(&state, user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Some(id) => id,
        None => {
            let id = create_express_account(&state.stripe_key, &email)
                .await
                .map_err(|_| StatusCode::BAD_GATEWAY)?;
            save_mentor_stripe_account(&state, user_id, &id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            id
        }
    };

    let url = create_account_link(
        &state.stripe_key,
        &account_id,
        &req.refresh_url,
        &req.return_url,
    )
    .await
    .map_err(|_| StatusCode::BAD_GATEWAY)?;

    Ok(Json(ConnectOnboardResponse {
        onboarding_url: url,
        account_id,
    }))
}

pub async fn connect_status(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<ConnectStatusResponse>, StatusCode> {
    let user_id = headers
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let account_id: Option<String> =
        sqlx::query_scalar("SELECT stripe_connect_account_id FROM users WHERE id = $1::uuid")
            .bind(user_id)
            .fetch_one(&state.general_pool)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Json(ConnectStatusResponse {
        connected: account_id.is_some(),
        account_id,
    }))
}

async fn mentor_stripe_account(
    state: &SharedState,
    mentor_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT stripe_connect_account_id FROM users WHERE id = $1::uuid",
    )
    .bind(mentor_id)
    .fetch_optional(&state.general_pool)
    .await
}

async fn save_mentor_stripe_account(
    state: &SharedState,
    user_id: &str,
    account_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET stripe_connect_account_id = $1, updated_at = NOW() WHERE id = $2::uuid",
    )
    .bind(account_id)
    .bind(user_id)
    .execute(&state.general_pool)
    .await?;
    Ok(())
}

async fn activate_enrollment(users_url: &str, enrollment_id: &str, session_id: &str) {
    let client = reqwest::Client::new();
    let url = format!("{}/enrollments/{}/activate", users_url, enrollment_id);
    let _ = client
        .patch(&url)
        .json(&serde_json::json!({"checkout_session_id": session_id}))
        .send()
        .await;
}
