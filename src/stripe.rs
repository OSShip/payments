use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

pub fn verify_signature(secret: &str, payload: &str, sig_header: &str) -> bool {
    let mut timestamp = "";
    let mut v1 = "";
    for part in sig_header.split(',') {
        let mut kv = part.splitn(2, '=');
        match (kv.next(), kv.next()) {
            (Some("t"), Some(v)) => timestamp = v,
            (Some("v1"), Some(v)) => v1 = v,
            _ => {}
        }
    }
    if timestamp.is_empty() || v1.is_empty() {
        return false;
    }
    let signed = format!("{}.{}", timestamp, payload);
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(signed.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());
    expected.as_bytes().ct_eq(v1.as_bytes()).into()
}

pub async fn create_checkout_session(
    stripe_key: &str,
    amount_cents: i64,
    fee_cents: i64,
    mentor_account: Option<&str>,
    success_url: &str,
    cancel_url: &str,
    listing_id: &str,
    student_id: &str,
    mentor_id: &str,
    enrollment_id: &str,
) -> Result<serde_json::Value, reqwest::Error> {
    let client = reqwest::Client::new();
    let amount = amount_cents.to_string();
    let fee = fee_cents.to_string();
    let mut params = vec![
        ("mode", "payment"),
        ("success_url", success_url),
        ("cancel_url", cancel_url),
        ("line_items[0][price_data][currency]", "usd"),
        ("line_items[0][price_data][unit_amount]", &amount),
        (
            "line_items[0][price_data][product_data][name]",
            "Mentorship Slot",
        ),
        ("line_items[0][quantity]", "1"),
        ("metadata[listing_id]", listing_id),
        ("metadata[student_id]", student_id),
        ("metadata[mentor_id]", mentor_id),
        ("metadata[enrollment_id]", enrollment_id),
    ];

    let account;
    if let Some(acct) = mentor_account {
        account = acct.to_string();
        params.push(("payment_intent_data[application_fee_amount]", &fee));
        params.push(("payment_intent_data[transfer_data][destination]", &account));
    }

    client
        .post("https://api.stripe.com/v1/checkout/sessions")
        .basic_auth(stripe_key, None::<&str>)
        .form(&params)
        .send()
        .await?
        .json()
        .await
}

pub async fn create_express_account(
    stripe_key: &str,
    email: &str,
) -> Result<String, reqwest::Error> {
    let client = reqwest::Client::new();
    let body: serde_json::Value = client
        .post("https://api.stripe.com/v1/accounts")
        .basic_auth(stripe_key, None::<&str>)
        .form(&[
            ("business_type", "individual"),
            ("email", email),
            ("capabilities[card_payments][requested]", "true"),
            ("capabilities[transfers][requested]", "true"),
        ])
        .send()
        .await?
        .json()
        .await?;
    Ok(body["id"].as_str().unwrap_or_default().to_string())
}

pub async fn create_account_link(
    stripe_key: &str,
    account_id: &str,
    refresh_url: &str,
    return_url: &str,
) -> Result<String, reqwest::Error> {
    let client = reqwest::Client::new();
    let body: serde_json::Value = client
        .post("https://api.stripe.com/v1/account_links")
        .basic_auth(stripe_key, None::<&str>)
        .form(&[
            ("account", account_id),
            ("refresh_url", refresh_url),
            ("return_url", return_url),
            ("type", "account_onboarding"),
        ])
        .send()
        .await?
        .json()
        .await?;
    tracing::info!("Stripe api response {}", &body);
    Ok(body["url"].as_str().unwrap_or_default().to_string())
}
