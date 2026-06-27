use crate::sentry_util;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

pub async fn enqueue_outbox(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO payment_outbox (event_type, payload) VALUES ($1, $2)",
    )
    .bind(event_type)
    .bind(payload)
    .execute(executor)
    .await?;
    Ok(())
}

pub fn spawn_outbox_worker(pool: PgPool, brokers: String) {
    tokio::spawn(async move {
        let producer: FutureProducer = match ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("message.timeout.ms", "5000")
            .create()
        {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("kafka producer init failed: {}", e);
                sentry_util::capture_error(&e, &[("worker", "outbox"), ("stage", "kafka_init")]);
                return;
            }
        };

        loop {
            if let Err(e) = publish_batch(&pool, &producer).await {
                tracing::warn!("outbox publish error: {}", e);
                sentry_util::capture_error(e.as_ref(), &[("worker", "outbox"), ("stage", "publish_batch")]);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn publish_batch(
    pool: &PgPool,
    producer: &FutureProducer,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rows: Vec<(Uuid, String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, event_type, payload FROM payment_outbox WHERE NOT published ORDER BY created_at LIMIT 20",
    )
    .fetch_all(pool)
    .await?;

    for (id, event_type, payload) in rows {
        let event_id = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%d%H%M%S"),
            &id.to_string()[..8]
        );
        let event = serde_json::json!({
            "event_id": event_id,
            "type": event_type,
            "timestamp": chrono::Utc::now(),
            "payload": payload,
            "schema_version": "1.0"
        });
        let body = serde_json::to_string(&event)?;
        let record = FutureRecord::to("payment.events").payload(&body).key(&event_id);
        match producer.send(record, Duration::from_secs(5)).await {
            Ok(_) => {
                sqlx::query("UPDATE payment_outbox SET published = TRUE WHERE id = $1")
                    .bind(id)
                    .execute(pool)
                    .await?;
            }
            Err((e, _)) => {
                tracing::warn!("kafka send failed for outbox {}: {}", id, e);
                sentry_util::capture_error(&e, &[("worker", "outbox"), ("stage", "kafka_send")]);
            }
        }
    }
    Ok(())
}
