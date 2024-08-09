//! # axum-insights
//! 
//! An [Azure Application Insights](https://docs.microsoft.com/en-us/azure/azure-monitor/app/app-insights-overview) 
//! exporter for [axum](https://github.com/tokio-rs/axum) via [`tracing`](https://github.com/tokio-rs/tracing).
//! 
//! ## Example
//! 
//! The following example is a "complete" example, which means that it includes all of the optional features of this library.
//! 
//! ```
//! use serde::{Serialize, Deserialize};
//! use axum::Router;
//! use axum_insights::AppInsights;
//! use axum_insights::AppInsightsError;
//! use tracing_subscriber::filter::LevelFilter;
//! use std::collections::HashMap;
//! use tracing::Instrument;
//! 
//! // Define some helper types for the example.
//! 
//! #[derive(Default, Serialize, Deserialize, Clone)]
//! struct WebError {
//!     message: String,
//! }
//! 
//! impl AppInsightsError for WebError {
//!     fn message(&self) -> Option<String> {
//!         Some(self.message.clone())
//!     }
//! 
//!     fn backtrace(&self) -> Option<String> {
//!         None
//!     }
//! }
//! 
//! // Set up the exporter, and get the `tower::Service` layer.
//! 
//! let telemetry_layer = AppInsights::default()
//!     // Accepts an optional connection string.  If None, then no telemetry is sent.
//!     .with_connection_string(None)
//!     // Sets the service namespace and name.  Default is empty.
//!     .with_service_config("namespace", "name")
//!     // Sets the HTTP client to use for sending telemetry.  Default is reqwest async client.
//!     .with_client(reqwest::Client::new())
//!     // Sets whether or not live metrics are collected.  Default is false.
//!     .with_live_metrics(true)
//!     // Sets the sample rate for telemetry.  Default is 1.0.
//!     .with_sample_rate(1.0)
//!     // Sets the minimum level for telemetry.  Default is INFO.
//!     .with_minimum_level(LevelFilter::INFO)
//!     // Sets the subscriber to use for telemetry.  Default is a new subscriber.
//!     .with_subscriber(tracing_subscriber::registry())
//!     // Sets the runtime to use for telemetry.  Default is Tokio.
//!     .with_runtime(opentelemetry_sdk::runtime::Tokio)
//!     // Sets whether or not to catch panics, and emit a trace for them.  Default is false.
//!     .with_catch_panic(true)
//!     // Sets whether or not to make this telemetry layer a noop.  Default is false.
//!     .with_noop(true)
//!     // Sets a function to extract extra fields from the request.  Default is no extra fields.
//!     .with_field_mapper(|parts| {
//!         let mut map = HashMap::new();
//!         map.insert("extra_field".to_owned(), "extra_value".to_owned());
//!         map
//!     })
//!     // Sets a function to extract extra fields from a panic.  Default is a default error.
//!     .with_panic_mapper(|panic| {
//!         (500, WebError { message: panic })
//!     })
//!     // Sets a function to determine the success-iness of a status.  Default is (100 - 399 => true).
//!     .with_success_filter(|status| {
//!         status.is_success() || status.is_redirection() || status.is_informational() || status == http::StatusCode::NOT_FOUND
//!     })
//!     // Sets the common error type for the application, and will automatically extract information from handlers that return that error.
//!     .with_error_type::<WebError>()
//!     .build_and_set_global_default()
//!     .unwrap()
//!     .layer();
//! 
//! // Add the layer to your app.
//! 
//! // You likely will not need to specify `Router<()>` in your implementation.  This is just for the example.
//! let app: Router<()> = Router::new()
//!     // ...
//!     .layer(telemetry_layer);
//! 
//! // Then, in a handler, you would use the `tracing` macros to emit telemetry.
//! 
//! use axum::response::IntoResponse;
//! use axum::Json;
//! use tracing::{Level, instrument, debug, error, info, warn, event};
//! 
//! // Instrument async handlers to get method-specific tracing.
//! #[instrument]
//! async fn handler(Json(body): Json<String>) -> Result<impl IntoResponse, WebError> {
//!     // Emit events using the `tracing` macros.
//!     debug!("Debug message");
//!     info!("Info message");
//!     warn!("Warn message");
//!     error!("Error message");
//!     event!(name: "exception", Level::ERROR, exception.message = "error message");
//! 
//!     // Create new spans using the `tracing` macros.
//!     let span = tracing::info_span!("DB Query");
//!     
//!     db_query().instrument(span).await;
//!     
//!     if body == "error" {
//!         return Err(WebError { message: "Error".to_owned() });
//!     }
//! 
//!     Ok(())
//! }
//! 
//! async fn db_query() {
//!     // ...
//! }
//! ```


// Directives.

#![warn(rustdoc::broken_intra_doc_links, rust_2018_idioms, clippy::all, missing_docs)]

use std::{
    backtrace::Backtrace,
    collections::HashMap,
    error::Error,
    panic::{self, AssertUnwindSafe},
    sync::Arc,
    task::{Context, Poll},
};

use axum::{extract::MatchedPath, response::Response, RequestPartsExt, body::Body};
use futures::{future::BoxFuture, FutureExt};
use http::StatusCode;
use http_body_util::BodyExt;
use hyper::Request;
use opentelemetry::KeyValue;
use opentelemetry_sdk::{runtime::{RuntimeChannel, Tokio}, trace::Config};
use opentelemetry_application_insights::HttpClient;
use reqwest::Client;
use serde::{de::DeserializeOwned, Serialize};
use tower::{Layer, Service};
use tracing::{Instrument, Span, Level};
use tracing_subscriber::{filter::LevelFilter, prelude::__tracing_subscriber_SubscriberExt, Registry};

// Re-exports.

/// Re-exports of the dependencies of this crate.
/// 
/// Generally, you can use some of these modules to get at relevant types you may need.
/// One big exception is proc-macros such as `#[instrument]`, which are not re-exported.
/// In those cases, you will need to explicitly add a dependency for [`tracing`](https://github.com/tokio-rs/tracing).
pub mod exports {
    pub use opentelemetry;
    pub use opentelemetry_application_insights;
    pub use reqwest;
    pub use serde;
    pub use tokio;
    pub use tracing;
    pub use tracing_opentelemetry;
    pub use tracing_subscriber;
}

// Traits.

/// A trait that extracts relevant information from a global error type.
/// 
/// A type that implements this trait can be used as the `E` type parameter for [`AppInsights`].
/// It is usually set via [`AppInsights::with_error_type`].
/// 
/// ```
/// use axum_insights::AppInsights;
/// use axum_insights::AppInsightsError;
/// 
/// struct WebError {
///     message: String,
/// }
/// 
/// impl AppInsightsError for WebError {
///     fn message(&self) -> Option<String> {
///         Some(self.message.clone())
///     }
/// 
///     fn backtrace(&self) -> Option<String> {
///         None
///     }
/// }
/// 
/// let telemetry_layer = AppInsights::default()
///     .with_connection_string(None)
///     .with_service_config("namespace", "name")
///     .with_error_type::<WebError>()
///     .build_and_set_global_default()
///     .unwrap()
///     .layer();
/// ```
/// 
/// If your handlers all return a type that implements this trait, then you can use the [`AppInsightsLayer`] to automatically
/// instrument all of your handlers to extract some of the error information from your error type (only attempts the extraction
/// for 400s and 500s).
/// 
/// Implementing this trait allows the [`AppInsightsLayer`] to extract the error message and backtrace from your error type,
/// and add that information to the resulting traces.
pub trait AppInsightsError {
    /// The message of the error.
    fn message(&self) -> Option<String>;
    /// The backtrace of the error.
    fn backtrace(&self) -> Option<String>;
}

impl AppInsightsError for () {
    fn message(&self) -> Option<String> {
        None
    }

    fn backtrace(&self) -> Option<String> {
        None
    }
}

// Types.

/// The base state of the [`AppInsights`] builder struct.
pub struct Base;

/// The state of the [`AppInsights`] builder struct after a connection string has been set.
pub struct WithConnectionString;

/// The state of the [`AppInsights`] builder struct after a connection string and service config have been set.
pub struct Ready;

type OptionalPanicMapper<E> = Option<Arc<dyn Fn(String) -> (u16, E) + Send + Sync + 'static>>;
type OptionalFieldMapper = Option<Arc<dyn Fn(&http::request::Parts) -> HashMap<String, String> + Send + Sync + 'static>>;
type OptionalSuccessFilter = Option<Arc<dyn Fn(StatusCode) -> bool + Send + Sync + 'static>>;

/// The complete [`AppInsights`] builder struct.
/// 
/// This struct is returned from [`AppInsights::build_and_set_global_default`], and it is used to create the [`AppInsightsLayer`].
pub struct AppInsightsComplete<P, E> {
    is_noop: bool,
    field_mapper: OptionalFieldMapper,
    panic_mapper: OptionalPanicMapper<P>,
    success_filter: OptionalSuccessFilter,
    _phantom: std::marker::PhantomData<E>,
}

/// The main telemetry struct.
/// 
/// Refer to the top-level documentation for usage information.
pub struct AppInsights<S = Base, C = Client, R = Tokio, U = Registry, P = (), E = ()> {
    connection_string: Option<String>,
    config: Config,
    client: C,
    enable_live_metrics: bool,
    sample_rate: f64,
    batch_runtime: R,
    minimum_level: LevelFilter,
    subscriber: Option<U>,
    should_catch_panic: bool,
    is_noop: bool,
    field_mapper: OptionalFieldMapper,
    panic_mapper: OptionalPanicMapper<P>,
    success_filter: OptionalSuccessFilter,
    _phantom1: std::marker::PhantomData<S>,
    _phantom2: std::marker::PhantomData<E>,
}

impl Default for AppInsights<Base> {
    fn default() -> Self {
        Self {
            connection_string: None,
            config: Config::default(),
            client: Client::new(),
            enable_live_metrics: false,
            sample_rate: 1.0,
            batch_runtime: Tokio,
            minimum_level: LevelFilter::INFO,
            subscriber: None,
            should_catch_panic: false,
            is_noop: false,
            field_mapper: None,
            panic_mapper: None,
            success_filter: None,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }
}

impl<C, R, U, P, E> AppInsights<Base, C, R, U, P, E> {
    /// Sets the connection string to use for telemetry.
    /// 
    /// If this is not set, then no telemetry will be sent.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, WithConnectionString};
    /// 
    /// let i: AppInsights<WithConnectionString> = AppInsights::default()
    ///     .with_connection_string(None);
    /// ```
    pub fn with_connection_string(self, connection_string: impl Into<Option<String>>) -> AppInsights<WithConnectionString, C, R, U, P, E> {
        AppInsights {
            connection_string: connection_string.into(),
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }
}

impl<C, R, U, P, E> AppInsights<WithConnectionString, C, R, U, P, E> {
    /// Sets the service namespace and name.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name");
    /// ```
    /// 
    /// This is a convenience method for [`AppInsights::with_trace_config`].
    pub fn with_service_config(self, namespace: impl AsRef<str>, name: impl AsRef<str>) -> AppInsights<Ready, C, R, U, P> {
        let config = Config::default().with_resource(opentelemetry_sdk::Resource::new(vec![
            KeyValue::new("service.namespace", namespace.as_ref().to_owned()),
            KeyValue::new("service.name", name.as_ref().to_owned()),
        ]));

        AppInsights {
            connection_string: self.connection_string,
            config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets the trace config to use for telemetry.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// use opentelemetry_sdk::trace::Config;
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_trace_config(Config::default());
    /// ```
    pub fn with_trace_config(self, config: Config) -> AppInsights<Ready, C, R, U, P> {
        AppInsights {
            connection_string: self.connection_string,
            config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }
}

impl<C, R, U, P, E> AppInsights<Ready, C, R, U, P, E> {
    /// Sets the HTTP client to use for sending telemetry.  The default is reqwest async client.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_client(reqwest::Client::new());
    /// ```
    pub fn with_client(self, client: C) -> AppInsights<Ready, C, R, U, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets whether or not live metrics should be collected.  The default is false.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_client(reqwest::Client::new())
    ///     .with_live_metrics(true);
    /// ```
    pub fn with_live_metrics(self, should_collect_live_metrics: bool) -> AppInsights<Ready, C, R, U, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: should_collect_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets the sample rate for telemetry.  The default is 1.0.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_sample_rate(1.0);
    /// ```
    pub fn with_sample_rate(self, sample_rate: f64) -> AppInsights<Ready, C, R, U, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets the minimum level for telemetry.  The default is INFO.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// use tracing_subscriber::filter::LevelFilter;
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_minimum_level(LevelFilter::INFO);
    /// ```
    pub fn with_minimum_level(self, minimum_level: LevelFilter) -> AppInsights<Ready, C, R, U, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets the subscriber to use for telemetry.  The default is a new subscriber.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// use tracing_subscriber::Registry;
    /// 
    /// let i = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_subscriber(tracing_subscriber::registry());
    /// ```
    pub fn with_subscriber<T>(self, subscriber: T) -> AppInsights<Ready, C, R, T, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: Some(subscriber),
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets the runtime to use for the telemetry batch exporter.  The default is Tokio.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// use opentelemetry_sdk::runtime::Tokio;
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_runtime(Tokio);
    /// ```
    pub fn with_runtime<T>(self, runtime: T) -> AppInsights<Ready, C, T, U, P, E>
    where
        T: RuntimeChannel,
    {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets whether or not to catch panics, and emit a trace for them.  The default is false.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_catch_panic(true);
    /// ```
    pub fn with_catch_panic(self, should_catch_panic: bool) -> AppInsights<Ready, C, R, U, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets whether or not to make this telemetry layer a noop.  The default is false.
    /// 
    /// This is useful whenever you are running axum tests, as the global subscriber cannot be
    /// set in a multiple times.  Effectively, this causes the telemetry layer to be a no-op.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// 
    /// let i = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_noop(true);
    /// ```
    pub fn with_noop(self, should_noop: bool) -> AppInsights<Ready, C, R, U, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: should_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets a function to extract extra fields from the request.  The default is no extra fields.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// use std::collections::HashMap;
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_field_mapper(|parts| {
    ///         let mut map = HashMap::new();
    ///         map.insert("extra_field".to_owned(), "extra_value".to_owned());
    ///         map
    ///     });
    /// ```
    pub fn with_field_mapper<F>(self, field_mapper: F) -> AppInsights<Ready, C, R, U, P, E>
    where
        F: Fn(&http::request::Parts) -> HashMap<String, String> + Send + Sync + 'static,
    {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: Some(Arc::new(field_mapper)),
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets a function to extract extra fields from a panic.  The default is a default error.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// 
    /// struct WebError {
    ///     message: String,
    /// }
    /// 
    /// let i = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_panic_mapper(|panic| {
    ///         (500, WebError { message: panic })
    ///     });
    /// ```
    pub fn with_panic_mapper<F, T>(self, panic_mapper: F) -> AppInsights<Ready, C, R, U, T, E>
    where
        F: Fn(String) -> (u16, T) + Send + Sync + 'static,
    {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: Some(Arc::new(panic_mapper)),
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets a function to determine the success-iness of a status.  The default is (100 - 399 => true).
    /// 
    /// This allows you to fine-tune which statuses are considered successful, and which are not.  If you have
    /// lots of spurious 404s, for example, you can add that to the success statuses.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// use http::StatusCode;
    /// 
    /// let i = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_success_filter(|status| {
    ///         status.is_success() || status.is_redirection() || status.is_informational() || status == StatusCode::NOT_FOUND
    ///     });
    /// ```
    pub fn with_success_filter<F>(self, success_filter: F) -> AppInsights<Ready, C, R, U, P, E>
    where
        F: Fn(StatusCode) -> bool + Send + Sync + 'static,
    {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: Some(Arc::new(success_filter)),
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets the error type to use for telemetry.  The default is ().
    /// 
    /// ```
    /// use axum_insights::{AppInsights, AppInsightsError, Ready};
    /// 
    /// struct WebError {
    ///     message: String,
    /// }
    /// 
    /// impl AppInsightsError for WebError {
    ///     fn message(&self) -> Option<String> {
    ///         Some(self.message.clone())
    ///     }
    /// 
    ///     fn backtrace(&self) -> Option<String> {
    ///         None
    ///     }
    /// }
    /// 
    /// let i = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_error_type::<WebError>();
    /// ```
    pub fn with_error_type<T>(self) -> AppInsights<Ready, C, R, U, P, T> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            enable_live_metrics: self.enable_live_metrics,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Builds the telemetry layer, and sets it as the global default.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, AppInsightsComplete};
    /// 
    /// let i: AppInsightsComplete<_, _> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .build_and_set_global_default()
    ///     .unwrap();
    /// ```
    /// 
    /// The global default currently has to be set by this library.  If you want to use other subscribers,
    /// then you need to use [`AppInsights::with_subscriber`] to inject that subscriber, and then
    /// allow this call to set the global default.
    pub fn build_and_set_global_default(self) -> Result<AppInsightsComplete<P, E>, Box<dyn Error + Send + Sync + 'static>>
    where
        C: HttpClient + 'static,
        R: RuntimeChannel,
        U: tracing_subscriber::layer::SubscriberExt + for<'span> tracing_subscriber::registry::LookupSpan<'span>  + Send + Sync + 'static
    {
        if self.is_noop {
            return Ok(AppInsightsComplete {
                is_noop: true,
                field_mapper: None,
                panic_mapper: None,
                success_filter: None,
                _phantom: std::marker::PhantomData,
            });
        }

        // This subscriber calculation needs to be separate in order to allow the type inference to work properly.
        // Theoretically, we could do some magic with boxed traits to make it more readable, but this makes the types
        // work nicely.
        match self.subscriber {
            Some(subscriber) => {
                if let Some(connection_string) = self.connection_string {
                    let tracer = opentelemetry_application_insights::new_pipeline_from_connection_string(connection_string)?
                        .with_client(self.client)
                        .with_live_metrics(self.enable_live_metrics)
                        .with_trace_config(self.config)
                        .with_sample_rate(self.sample_rate)
                        .install_batch(self.batch_runtime);

                    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
                    let subscriber = subscriber.with(telemetry).with(self.minimum_level);
                    tracing::subscriber::set_global_default(subscriber)?;
                } else {
                    tracing::subscriber::set_global_default(subscriber.with(self.minimum_level))?;
                }
            },
            None => {
                if let Some(connection_string) = self.connection_string {
                    let tracer = opentelemetry_application_insights::new_pipeline_from_connection_string(connection_string)?
                        .with_client(self.client)
                        .with_live_metrics(self.enable_live_metrics)
                        .with_trace_config(self.config)
                        .with_sample_rate(self.sample_rate)
                        .install_batch(self.batch_runtime);

                    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
                    let subscriber = tracing_subscriber::registry().with(telemetry).with(self.minimum_level);
                    tracing::subscriber::set_global_default(subscriber)?;
                } else {
                    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(self.minimum_level))?;
                }
            },
        }

        if self.should_catch_panic {
            let default_panic = panic::take_hook();

            panic::set_hook(Box::new(move |p| {
                let payload_string = format!("{:?}", p.payload().downcast_ref::<&str>());
                let backtrace = Backtrace::force_capture().to_string();

                // This doesn't work because this macro prescribes the name without allowing it to be overriden.
                tracing::event!(
                    name: "exception",
                    Level::ERROR,
                    ai.customEvent.name = "exception",
                    "exception.type" = "PANIC",
                    exception.message = payload_string,
                    exception.stacktrace = backtrace
                );

                default_panic(p);
            }));
        }

        Ok(AppInsightsComplete {
            is_noop: false,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom: std::marker::PhantomData,
        })
    }
}

impl<P, E> AppInsightsComplete<P, E> {
    /// Creates the telemetry layer.
    /// 
    /// ```
    /// use axum::Router;
    /// use axum_insights::{AppInsights, AppInsightsComplete};
    /// 
    /// let i: AppInsightsComplete<_, _> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .build_and_set_global_default()
    ///     .unwrap();
    /// 
    /// let layer = i.layer();
    /// 
    /// // You likely will not need to specify `Router<()>` in your implementation.  This is just for the example.
    /// let app: Router<()> = Router::new()
    ///     // ...
    ///     .layer(layer);
    /// ```
    pub fn layer(self) -> AppInsightsLayer<P, E> {
        AppInsightsLayer {
            is_noop: self.is_noop,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            success_filter: self.success_filter,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// The telemetry layer.
/// 
/// This layer is created by [`AppInsightsComplete::layer`], and it can be used to instrument your [`axum::Router`].
/// Generally, this type will not be used, other than to pass to [`axum::Router::layer`].
#[derive(Clone)]
pub struct AppInsightsLayer<P, E> {
    is_noop: bool,
    field_mapper: OptionalFieldMapper,
    panic_mapper: OptionalPanicMapper<P>,
    success_filter: OptionalSuccessFilter,
    _phantom: std::marker::PhantomData<E>,
}

impl<S, P, E> Layer<S> for AppInsightsLayer<P, E> {
    type Service = AppInsightsMiddleware<S, P, E>;

    fn layer(&self, inner: S) -> Self::Service {
        AppInsightsMiddleware {
            inner,
            is_noop: self.is_noop,
            field_mapper: self.field_mapper.clone(),
            panic_mapper: self.panic_mapper.clone(),
            success_filter: self.success_filter.clone(),
            _phantom: std::marker::PhantomData,
        }
    }
}

/// The telemetry middleware.
/// 
/// This middleware is created by [`AppInsightsLayer::layer`], and it can be used to instrument your [`axum::Router`].
/// Generally, this type will not be used at all, is it merely satisfies the requirement that [`Layer::Service`]
/// is a [`Service`].
#[derive(Clone)]
pub struct AppInsightsMiddleware<S, P, E> {
    inner: S,
    is_noop: bool,
    field_mapper: OptionalFieldMapper,
    panic_mapper: OptionalPanicMapper<P>,
    success_filter: OptionalSuccessFilter,
    _phantom: std::marker::PhantomData<E>,
}

impl<S, P, E> Service<Request<Body>> for AppInsightsMiddleware<S, P, E>
where
    S: Service<Request<Body>, Response = Response> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    P: Serialize + Send + 'static,
    E: AppInsightsError + Serialize + DeserializeOwned + Default + Send + 'static,
{
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    type Response = S::Response;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        if self.is_noop {
            return Box::pin(self.inner.call(request));
        }

        // Get all of the basic request information.
        let method = request.method().to_string();
        let uri = request.uri().to_string();
        let client_ip = request.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()).unwrap_or("unknown").to_string();
        let client_ip = client_ip.split(',').next().unwrap_or("unknown");

        // Spit the request into parts, and extract the route, and any extra fields.
        let (mut parts, body) = request.into_parts();
        let route = futures::executor::block_on(parts.extract::<MatchedPath>())
            .map(|m| m.as_str().to_owned())
            .unwrap_or_else(|_| "unknown".to_owned());
        let extra_fields = self.field_mapper.as_ref().map(|f| f(&parts)).unwrap_or_default();

        // Put the request back together.
        let request = Request::from_parts(parts, body);

        // Create the span for the request, and leave empty fields for the response records.
        let span = tracing::info_span!(
            "request",
            otel.kind = "server",
            http.request.method = method.as_str(),
            url.full = uri.as_str(),
            client.address = client_ip,
            http.route = route.as_str(),
            http.response.status_code = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_message = tracing::field::Empty,
            extra_fields = serde_json::to_string_pretty(&extra_fields).unwrap()
        );

        // Clone the panic mapper so that it can be used in the future.
        let panic_mapper = self.panic_mapper.clone();
        let success_filter = self.success_filter.clone();

        // Kick off the request.
        let future = self.inner.call(request);

        // Create the pinned future that is the essence of this middleware after the response.
        Box::pin(
            async move {
                // Get the response, and catch any panics.
                let response = AssertUnwindSafe(future).catch_unwind().instrument(Span::current()).await;

                let response = match response {
                    Ok(response) => response,
                    Err(e) => {
                        // Get the payload string from the panic (usually the panic message).
                        let payload_string = format!("{:?}", e.downcast_ref::<&str>());

                        // Use the given mapper, or create a default error.  For now, a feature of this library is to "panic handle".
                        let (status, error_string) = if let Some(panic_mapper) = panic_mapper.as_ref() {
                            let (status, error) = panic_mapper(payload_string.clone());

                            (status, serde_json::to_string(&error).unwrap())
                        } else {
                            (
                                500,
                                format!(
                                    r#"{{
                                    "status": 500,
                                    "message": "A panic occurred: {}.",
                                }}"#,
                                    payload_string
                                )
                                .to_string(),
                            )
                        };

                        // Build a response for the error in the panic case.
                        Ok(Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(Body::from(error_string))
                            .unwrap())
                    }
                }?;

                // Get the response status information, and determine success.
                let status = response.status();

                let is_success = success_filter.as_ref().map(|f| f(status)).unwrap_or_else(|| status.is_success() || status.is_redirection() || status.is_informational());

                // Get the span information about the response.
                let (response, otel_status, otel_status_message) = if is_success {
                    // The happy path!
                    (response, "OK", format!(r#"{{ "status": {} }}"#, status.as_u16()))
                } else {
                    // Extract the error from the response, so we can get some data for the response part of the span.

                    // Breakup the response into parts.
                    let (parts, body) = response.into_parts();

                    // Get the body bytes.
                    let body_bytes = body.collect().await.unwrap_or_default().to_bytes();

                    // Deserialize the error.
                    let error: E = serde_json::from_slice(&body_bytes).unwrap_or_default();

                    // Get the stringified error.
                    let error_string = serde_json::to_string_pretty(&error).unwrap();

                    // This doesn't work because this macro prescribes the name without allowing it to be overriden.
                    tracing::event!(
                        name: "exception",
                        Level::ERROR,
                        ai.customEvent.name = "exception",
                        "exception.type" = format!("HTTP {}", status.as_u16()),
                        exception.message = error.message().unwrap_or_default(),
                        exception.stacktrace = error.backtrace().unwrap_or_default()
                    );

                    // Recreate the body.
                    let body = Body::from(body_bytes);

                    // Recreate the response.
                    let response = Response::from_parts(parts, body);

                    (response, "ERROR", error_string)
                };

                // Finish the span.
                let span = Span::current().entered();

                span.record("http.response.status_code", status.as_u16());
                span.record("otel.status_code", otel_status);

                if otel_status != "OK" {
                    span.record("otel.status_message", otel_status_message);
                }

                Ok(response)
            }
            .instrument(span),
        )
    }
}

// Tests.

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Sender;

    use axum::{Router, routing::get, response::IntoResponse};
    use http::StatusCode;
    use serde::Deserialize;
    use tracing::{Subscriber, span};
    use tracing_subscriber::Layer;

    use super::*;

    #[derive(Clone, Default, Serialize, Deserialize)]
    struct WebError {
        status: u16,
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

    impl IntoResponse for WebError {
        fn into_response(self) -> Response {
            let code = StatusCode::from_u16(self.status).unwrap();
            let body = serde_json::to_string(&self).unwrap();

            (code, body).into_response()
        }
    }

    struct TestSubscriberLayer {
        sender: Sender<String>,
    }

    impl<S> Layer<S> for TestSubscriberLayer
    where
        S: Subscriber
    {
        fn on_new_span(&self, attrs: &span::Attributes<'_>, _id: &span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            self.sender.send(format!("new|{}", attrs.metadata().name())).unwrap();
        }

        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            self.sender.send(format!("event|{}", event.metadata().name())).unwrap();
        }

        fn on_record(&self, _id: &span::Id, values: &span::Record<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            self.sender.send(format!("record|{:?}", values)).unwrap();
        }

        fn on_close(&self, _id: span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            self.sender.send("close".to_string()).unwrap();
        }
    }

    #[tokio::test]
    async fn test_integration() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let subscriber = tracing_subscriber::registry().with(TestSubscriberLayer {
            sender: sender.clone(),
        });

        let i = AppInsights::default()
            .with_connection_string(None)
            .with_service_config("namespace", "name")
            .with_client(reqwest::Client::new())
            .with_sample_rate(1.0)
            .with_minimum_level(LevelFilter::INFO)
            .with_runtime(Tokio)
            .with_catch_panic(true)
            .with_subscriber(subscriber)
            .with_field_mapper(|_| {
                let mut map = HashMap::new();
                map.insert("extra_field".to_owned(), "extra_value".to_owned());
                map
            })
            .with_panic_mapper(|panic| {
                (500, WebError { status: 500, message: panic })
            })
            .with_success_filter(|status| {
                status.is_success() || status.is_redirection() || status.is_informational() || status == StatusCode::NOT_FOUND
            })
            .with_error_type::<WebError>()
            .build_and_set_global_default()
            .unwrap();

        let layer = i.layer();

        let mut app: Router<()> = Router::new()
            .route("/succeed1", get(|| async { Response::new(Body::empty()) }))
            .route("/succeed2", get(|| async { (StatusCode::NOT_MODIFIED, "") }))
            .route("/succeed3", get(|| async { (StatusCode::NOT_FOUND, "") }))
            .route("/fail1", get(|| async { WebError { status: 429, message: "foo".to_string() } }))
            .route("/fail2", get(|| async { panic!("panic") }))
            .layer(layer);

        // Regular success.

        let request = Request::builder().uri("/succeed1").body(Body::empty()).unwrap();
        // This is required because there are multiple impls of `ready` for `Router`. 🙄
        let response = <axum::Router as tower::ServiceExt<Request<Body>>>::ready(&mut app).await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), 200);

        assert_eq!("new|request", receiver.recv().unwrap());
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { http.response.status_code: 200"));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { otel.status_code: \"OK\""));
        assert_eq!("close", receiver.recv().unwrap());

        // Redirect success.

        let request = Request::builder().uri("/succeed2").body(Body::empty()).unwrap();
        let response = <axum::Router as tower::ServiceExt<Request<Body>>>::ready(&mut app).await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), 304);

        assert_eq!("new|request", receiver.recv().unwrap());
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { http.response.status_code: 304"));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { otel.status_code: \"OK\""));
        assert_eq!("close", receiver.recv().unwrap());

        // Custom success.

        let request = Request::builder().uri("/succeed3").body(Body::empty()).unwrap();
        let response = <axum::Router as tower::ServiceExt<Request<Body>>>::ready(&mut app).await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), 404);

        assert_eq!("new|request", receiver.recv().unwrap());
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { http.response.status_code: 404"));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { otel.status_code: \"OK\""));
        assert_eq!("close", receiver.recv().unwrap());

        // Failure.

        let request = Request::builder().uri("/fail1").body(Body::empty()).unwrap();
        let response = <axum::Router as tower::ServiceExt<Request<Body>>>::ready(&mut app).await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), 429);

        assert_eq!("new|request", receiver.recv().unwrap());
        assert!(receiver.recv().unwrap().starts_with("event|exception"));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { http.response.status_code: 429"));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { otel.status_code: \"ERROR\""));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { otel.status_message: \"{\\n  \\\"status\\\": 429,\\n  \\\"message\\\": \\\"foo\\\"\\n}\""));
        assert_eq!("close", receiver.recv().unwrap());

        // Panic.

        let request = Request::builder().uri("/fail2").body(Body::empty()).unwrap();
        let response = <axum::Router as tower::ServiceExt<Request<Body>>>::ready(&mut app).await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), 500);

        assert_eq!("new|request", receiver.recv().unwrap());
        assert!(receiver.recv().unwrap().starts_with("event|exception"));
        assert!(receiver.recv().unwrap().starts_with("event|exception"));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { http.response.status_code: 500"));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { otel.status_code: \"ERROR\""));
        assert!(receiver.recv().unwrap().starts_with("record|Record { values: ValueSet { otel.status_message: \"{\\n  \\\"status\\\": 500,\\n  \\\"message\\\": \\\"Some(\\\\\\\"panic\\\\\\\")\\\"\\n}\""));
        assert_eq!("close", receiver.recv().unwrap());
    }

    #[tokio::test]
    async fn test_noop() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let subscriber = tracing_subscriber::registry().with(TestSubscriberLayer {
            sender: sender.clone(),
        });

        let i = AppInsights::default()
            .with_connection_string(None)
            .with_service_config("namespace", "name")
            .with_subscriber(subscriber)
            .with_noop(true)
            .build_and_set_global_default()
            .unwrap();

        let layer = i.layer();

        let mut app: Router<()> = Router::new()
            .route("/succeed1", get(|| async { Response::new(Body::empty()) }))
            .layer(layer);

        // Regular success.

        let request = Request::builder().uri("/succeed1").body(Body::empty()).unwrap();
        let response = <axum::Router as tower::ServiceExt<Request<Body>>>::ready(&mut app).await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), 200);

        assert!(receiver.try_recv().is_err());
    }
}