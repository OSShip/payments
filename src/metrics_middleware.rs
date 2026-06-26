use axum::{body::Body, http::Request, middleware::Next, response::Response};
use metrics::{counter, histogram};
use std::time::Instant;

pub async fn track(req: Request<Body>, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    counter!("http_requests_total", "service" => "payments", "method" => method.to_string(), "route" => path.clone(), "status_code" => status.to_string()).increment(1);
    histogram!("http_request_duration_seconds", "service" => "payments", "method" => method.to_string(), "route" => path).record(start.elapsed().as_secs_f64());
    response
}
