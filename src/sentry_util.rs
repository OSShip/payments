use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn init_sentry(service_name: &str) -> Option<sentry::ClientInitGuard> {
    let dsn = std::env::var("SENTRY_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;
    ENABLED.store(true, Ordering::Relaxed);

    let sample_rate: f32 = std::env::var("SENTRY_TRACES_SAMPLE_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.1);
    let environment = std::env::var("SENTRY_ENVIRONMENT")
        .unwrap_or_else(|_| "development".into());

    let guard = sentry::init(sentry::ClientOptions {
        dsn: Some(dsn.parse().ok()?),
        environment: Some(environment.into()),
        server_name: Some(service_name.to_string().into()),
        traces_sample_rate: sample_rate,
        ..Default::default()
    });
    Some(guard)
}

pub fn capture_error(err: &dyn std::error::Error, tags: &[(&str, &str)]) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    sentry::with_scope(
        |scope| {
            for (k, v) in tags {
                scope.set_tag(k, *v);
            }
        },
        || sentry::capture_error(err),
    );
}
