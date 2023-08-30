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

let telemetry_layer = AppInsights::default()
    .with_connection_string(None)                       // Accepts an optional connection string.  If None, then no telemetry is sent.
    .with_service_config("namespace", "name")           // Sets the service namespace and name.  Default is empty.
    .with_client(reqwest::Client::new())                // Sets the HTTP client to use for sending telemetry.  Default is reqwest async client.
    .with_sample_rate(1.0)                              // Sets the sample rate for telemetry.  Default is 1.0.
    .with_minimum_level(LevelFilter::INFO)              // Sets the minimum level for telemetry.  Default is INFO.
    .with_subscriber(tracing_subscriber::registry())    // Sets the subscriber to use for telemetry.  Default is a new subscriber.
    .with_runtime(opentelemetry::runtime::Tokio)        // Sets the runtime to use for telemetry.  Default is Tokio.
    .with_catch_panic(true)                             // Sets whether or not to catch panics, and emit a trace for them.  Default is false.
    .with_field_mapper(|parts| {                        // Sets a function to extract extra fields from the request.  Default is no extra fields.
        let mut map = HashMap::new();
        map.insert("extra_field".to_owned(), "extra_value".to_owned());
        map
    })
    .with_panic_mapper(|panic| {                        // Sets a function to extract extra fields from a panic.  Default is a default error.
        (500, WebError { message: panic })
    })
    .with_error_type::<WebError>()
    .build_and_set_global_default()
    .unwrap()
    .layer();

// You likely will not need to specify `Router<()>` in your implementation.  This is just for the example.
let app: Router<()> = Router::new()
    // ...
    .layer(telemetry_layer);

// Then, in a handler, you would use the `tracing` macros to emit telemetry.

use axum::response::IntoResponse;
use axum::Json;
use tracing::{instrument, debug, error, info, warn};

#[instrument]
async fn handler(Json(body): Json<String>) -> Result<impl IntoResponse, WebError> {
    debug!("Debug message");
    info!("Info message");
    warn!("Warn message");
    error!("Error message");
    
    if body == "error" {
        return Err(WebError { message: "Error".to_owned() });
    }

    Ok(())
}
```

## Test

```bash
cargo test --features web
```