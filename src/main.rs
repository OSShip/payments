mod handlers;
mod ledger;
mod metrics_middleware;
mod outbox;
mod sentry_util;
mod state;
mod stripe;

use axum::{
    routing::{get, post},
    Json, Router,
};
use metrics::describe_counter;
use sqlx::postgres::PgPoolOptions;
use state::{AppState, SharedState};
use std::sync::Arc;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() {
    let _sentry = sentry_util::init_sentry("payments");
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            sentry::integrations::tracing::layer()
                .event_filter(sentry_util::tracing_event_filter),
        )
        .init();
    if sentry_util::is_enabled() {
        tracing::info!(service = "payments", "sentry initialized with structured logs");
    }
    describe_counter!("ledger_writes_total", "Total ledger writes");
    describe_counter!("stripe_webhook_errors_total", "Stripe webhook errors");

    let database_url = std::env::var("DATABASE_URL_PAYMENTS").unwrap_or_else(|_| {
        "postgres://osship:osship_secret@postgres:5432/osship?sslmode=disable".into()
    });
    let general_url = std::env::var("DATABASE_URL_GENERAL").unwrap_or_else(|_| {
        "postgres://osship:osship_secret@postgres:5432/osship?sslmode=disable".into()
    });

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO payments")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("payments db connect");

    let general_pool = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO general")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&general_url)
        .await
        .expect("general db connect");

    let kafka_brokers =
        std::env::var("KAFKA_BROKERS").unwrap_or_else(|_| "kafka:9092".into());
    tracing::info!(brokers = %kafka_brokers, "starting outbox worker");
    outbox::spawn_outbox_worker(pool.clone(), kafka_brokers.clone());

    let state: SharedState = Arc::new(AppState {
        pool,
        general_pool,
        stripe_key: std::env::var("STRIPE_SECRET_KEY").unwrap_or_default(),
        webhook_secret: std::env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default(),
        platform_fee_percent: std::env::var("PLATFORM_FEE_PERCENT")
            .unwrap_or_else(|_| "10".into())
            .parse()
            .unwrap_or(10),
        users_url: std::env::var("USERS_SERVICE_URL")
            .unwrap_or_else(|_| "http://users:8083".into()),
        kafka_brokers,
        app_base_url: std::env::var("APP_BASE_URL")
            .unwrap_or_else(|_| "http://localhost".into()),
    });

    if !state.stripe_configured() {
        tracing::warn!("Stripe not configured, mock checkout mode enabled");
    } else {
        tracing::info!("Stripe configured for live checkout");
    }

    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("metrics recorder");

    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(move || async move { recorder.render() }))
        .route("/checkout", post(handlers::checkout))
        .route("/webhooks/stripe", post(handlers::stripe_webhook))
        .route("/ledger/{listing_id}", get(handlers::get_ledger))
        .route("/payout-summary", get(handlers::payout_summary_handler))
        .route("/connect/onboard", post(handlers::connect_onboard))
        .route("/connect/status", get(handlers::connect_status))
        .layer(axum::middleware::from_fn(metrics_middleware::track))
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "8087".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .unwrap();
    tracing::info!("payments listening on :{}", port);
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status":"ok","service":"payments"}))
}
