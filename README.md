[![Build and Test](https://github.com/twitchax/axum-insights/actions/workflows/build.yml/badge.svg)](https://github.com/twitchax/axum-insights/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/twitchax/axum-insights/branch/main/graph/badge.svg?token=35MZN0YFZF)](https://codecov.io/gh/twitchax/axum-insights)
[![Version](https://img.shields.io/crates/v/axum-insights.svg)](https://crates.io/crates/axum-insights)
[![Crates.io](https://img.shields.io/crates/d/axum-insights?label=crate)](https://crates.io/crates/axum-insights)
[![Documentation](https://docs.rs/axum-insights/badge.svg)](https://docs.rs/axum-insights)
[![Rust](https://img.shields.io/badge/rust-stable-blue.svg?maxAge=3600)](https://github.com/twitchax/axum-insights)
[![License:MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# axum-insights

An [Azure Application Insights](https://docs.microsoft.com/en-us/azure/azure-monitor/app/app-insights-overview) 
exporter for [axum](https://github.com/tokio-rs/axum) via [`tracing`](https://github.com/tokio-rs/tracing).

## Usage

This library is meant to be used as a layer for axum.  It will automatically instrument your axum application, and send telemetry to Azure Application Insights.
As the ecosystem matures, more features will be added.

## Example

The following example is a "complete" example, which means that it includes all of the optional features of this library.

```rust
use serde::{Serialize, Deserialize};
use axum::Router;
use axum_insights::AppInsights;
use axum_insights::AppInsightsError;
use tracing_subscriber::filter::LevelFilter;
use std::collections::HashMap;
use tracing::Instrument;

// Define some helper types for the example.

#[derive(Default, Serialize, Deserialize, Clone)]
struct WebError {
    message: String,
}

impl AppInsightsError for WebError {
    fn message(&self) -> Option<String> {
        Some(self.message.clone())
    }

    fn backtrace(&self) -> Option<String> {
        None
    }
}

// Set up the exporter, and get the `tower::Service` layer.

let telemetry_layer = AppInsights::default()
    // Accepts an optional connection string.  If None, then no telemetry is sent.
    .with_connection_string(None)
    // Sets the service namespace and name.  Default is empty.
    .with_service_config("namespace", "name")
    // Sets the HTTP client to use for sending telemetry.  Default is reqwest async client.
    .with_client(reqwest::Client::new())
    // Sets whether or not live metrics are collected.  Default is false.
    .with_live_metrics(true)
    // Sets the sample rate for telemetry.  Default is 1.0.
    .with_sample_rate(1.0)
    // Sets the minimum level for telemetry.  Default is INFO.
    .with_minimum_level(LevelFilter::INFO)
    // Sets the subscriber to use for telemetry.  Default is a new subscriber.
    .with_subscriber(tracing_subscriber::registry())
    // Sets the runtime to use for telemetry.  Default is Tokio.
    .with_runtime(opentelemetry::runtime::Tokio)
    // Sets whether or not to catch panics, and emit a trace for them.  Default is false.
    .with_catch_panic(true)
    // Sets whether or not to make this telemetry layer a noop.  Default is false.
    .with_noop(true)
    // Sets a function to extract extra fields from the request.  Default is no extra fields.
    .with_field_mapper(|parts| {
        let mut map = HashMap::new();
        map.insert("extra_field".to_owned(), "extra_value".to_owned());
        map
    })
    // Sets a function to extract extra fields from a panic.  Default is a default error.
    .with_panic_mapper(|panic| {
        (500, WebError { message: panic })
    })
    // Sets a function to determine the success-iness of a status.  Default is (100 - 399 => true).
    .with_success_filter(|status| {
        status.is_success() || status.is_redirection() || status.is_informational() || status == http::StatusCode::NOT_FOUND
    })
    // Sets the common error type for the application, and will automatically extract information from handlers that return that error.
    .with_error_type::<WebError>()
    .build_and_set_global_default()
    .unwrap()
    .layer();

// Add the layer to your app.

// You likely will not need to specify `Router<()>` in your implementation.  This is just for the example.
let app: Router<()> = Router::new()
    // ...
    .layer(telemetry_layer);

// Then, in a handler, you would use the `tracing` macros to emit telemetry.

use axum::response::IntoResponse;
use axum::Json;
use tracing::{Level, instrument, debug, error, info, warn, event};

// Instrument async handlers to get method-specific tracing.
#[instrument]
async fn handler(Json(body): Json<String>) -> Result<impl IntoResponse, WebError> {
    // Emit events using the `tracing` macros.
    debug!("Debug message");
    info!("Info message");
    warn!("Warn message");
    error!("Error message");
    event!(name: "exception", Level::ERROR, exception.message = "error message");

    // Create new spans using the `tracing` macros.
    let span = tracing::info_span!("DB Query");
    
    db_query().instrument(span).await;
    
    if body == "error" {
        return Err(WebError { message: "Error".to_owned() });
    }

    Ok(())
}

async fn db_query() {
    // ...
}
```

## Acknowledgements

This library depends on individual efforts of other maintainers such as:
* [opentelemetry-application-insights](https://github.com/frigus02/opentelemetry-application-insights) by [@frigus02](https://github.com/frigus02).
* [appinsights-rs](https://github.com/dmolokanov/appinsights-rs) by [@dmolokanov](https://github.com/dmolokanov).

## Test

```bash
cargo test --features web
```