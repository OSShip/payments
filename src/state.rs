use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub general_pool: PgPool,
    pub stripe_key: String,
    pub webhook_secret: String,
    pub platform_fee_percent: i32,
    pub users_url: String,
    pub kafka_brokers: String,
    pub app_base_url: String,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn stripe_configured(&self) -> bool {
        !self.stripe_key.is_empty() 
    }

    pub fn webhook_configured(&self) -> bool {
        !self.webhook_secret.is_empty() 
    }
}
