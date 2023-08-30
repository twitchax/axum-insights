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
//! let telemetry_layer = AppInsights::default()
//!     .with_connection_string(None)                       // Accepts an optional connection string.  If None, then no telemetry is sent.
//!     .with_service_config("namespace", "name")           // Sets the service namespace and name.  Default is empty.
//!     .with_client(reqwest::Client::new())                // Sets the HTTP client to use for sending telemetry.  Default is reqwest async client.
//!     .with_sample_rate(1.0)                              // Sets the sample rate for telemetry.  Default is 1.0.
//!     .with_minimum_level(LevelFilter::INFO)              // Sets the minimum level for telemetry.  Default is INFO.
//!     .with_subscriber(tracing_subscriber::registry())    // Sets the subscriber to use for telemetry.  Default is a new subscriber.
//!     .with_runtime(opentelemetry::runtime::Tokio)        // Sets the runtime to use for telemetry.  Default is Tokio.
//!     .with_catch_panic(true)                             // Sets whether or not to catch panics, and emit a trace for them.  Default is false.
//!     .with_field_mapper(|parts| {                        // Sets a function to extract extra fields from the request.  Default is no extra fields.
//!         let mut map = HashMap::new();
//!         map.insert("extra_field".to_owned(), "extra_value".to_owned());
//!         map
//!     })
//!     .with_panic_mapper(|panic| {                        // Sets a function to extract extra fields from a panic.  Default is a default error.
//!         (500, WebError { message: panic })
//!     })
//!     .with_error_type::<WebError>()
//!     .build_and_set_global_default()
//!     .unwrap()
//!     .layer();
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
//! use tracing::{instrument, debug, error, info, warn};
//! 
//! #[instrument]
//! async fn handler(Json(body): Json<String>) -> Result<impl IntoResponse, WebError> {
//!     debug!("Debug message");
//!     info!("Info message");
//!     warn!("Warn message");
//!     error!("Error message");
//!     
//!     if body == "error" {
//!         return Err(WebError { message: "Error".to_owned() });
//!     }
//! 
//!     Ok(())
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

use axum::{extract::MatchedPath, response::Response, RequestPartsExt};
use futures::{future::BoxFuture, FutureExt};
use hyper::{
    body::{Bytes, HttpBody},
    Body, Request,
};
use opentelemetry::{
    runtime::RuntimeChannel,
    sdk::{
        self,
        trace::{BatchMessage, Config},
    },
    KeyValue,
};
use opentelemetry_application_insights::HttpClient;
use reqwest::Client;
use serde::{de::DeserializeOwned, Serialize};
use tower::{Layer, Service};
use tracing::{Instrument, Span};
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

/// The complete [`AppInsights`] builder struct.
/// 
/// This struct is returned from [`AppInsights::build_and_set_global_default`], and it is used to create the [`AppInsightsLayer`].
pub struct AppInsightsComplete<P, E> {
    field_mapper: OptionalFieldMapper,
    panic_mapper: OptionalPanicMapper<P>,
    _phantom: std::marker::PhantomData<E>,
}

/// The main telemetry struct.
/// 
/// Refer to the top-level documentation for usage information.
pub struct AppInsights<S = Base, C = Client, R = opentelemetry::runtime::Tokio, P = (), E = ()>
where
    C: HttpClient + 'static,
    R: RuntimeChannel<BatchMessage>,
{
    connection_string: Option<String>,
    config: Config,
    client: C,
    sample_rate: f64,
    batch_runtime: R,
    minimum_level: LevelFilter,
    subscriber: Option<Registry>,
    should_catch_panic: bool,
    field_mapper: OptionalFieldMapper,
    panic_mapper: OptionalPanicMapper<P>,
    _phantom1: std::marker::PhantomData<S>,
    _phantom2: std::marker::PhantomData<E>,
}

impl Default for AppInsights<Base> {
    fn default() -> Self {
        Self {
            connection_string: None,
            config: Config::default(),
            client: Client::new(),
            sample_rate: 1.0,
            batch_runtime: opentelemetry::runtime::Tokio,
            minimum_level: LevelFilter::INFO,
            subscriber: None,
            should_catch_panic: false,
            field_mapper: None,
            panic_mapper: None,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }
}

impl<C, R, P, E> AppInsights<Base, C, R, P, E>
where
    C: HttpClient + 'static,
    R: RuntimeChannel<BatchMessage>,
{
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
    pub fn with_connection_string(self, connection_string: impl Into<Option<String>>) -> AppInsights<WithConnectionString, C, R, P, E> {
        AppInsights {
            connection_string: connection_string.into(),
            config: self.config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }
}

impl<C, R, P, E> AppInsights<WithConnectionString, C, R, P, E>
where
    C: HttpClient + 'static,
    R: RuntimeChannel<BatchMessage>,
{
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
    pub fn with_service_config(self, namespace: impl AsRef<str>, name: impl AsRef<str>) -> AppInsights<Ready, C, R, P> {
        let config = Config::default().with_resource(sdk::Resource::new(vec![
            KeyValue::new("service.namespace", namespace.as_ref().to_owned()),
            KeyValue::new("service.name", name.as_ref().to_owned()),
        ]));

        AppInsights {
            connection_string: self.connection_string,
            config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets the trace config to use for telemetry.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// use opentelemetry::sdk::trace::Config;
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_trace_config(Config::default());
    /// ```
    pub fn with_trace_config(self, config: Config) -> AppInsights<Ready, C, R, P> {
        AppInsights {
            connection_string: self.connection_string,
            config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }
}

impl<C, R, P, E> AppInsights<Ready, C, R, P, E>
where
    C: HttpClient + 'static,
    R: RuntimeChannel<BatchMessage>,
{
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
    pub fn with_client(self, client: C) -> AppInsights<Ready, C, R, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
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
    pub fn with_sample_rate(self, sample_rate: f64) -> AppInsights<Ready, C, R, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
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
    pub fn with_minimum_level(self, minimum_level: LevelFilter) -> AppInsights<Ready, C, R, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
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
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_subscriber(tracing_subscriber::registry());
    /// ```
    pub fn with_subscriber(self, subscriber: Registry) -> AppInsights<Ready, C, R, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: Some(subscriber),
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
            _phantom1: std::marker::PhantomData,
            _phantom2: std::marker::PhantomData,
        }
    }

    /// Sets the runtime to use for the telemetry batch exporter.  The default is Tokio.
    /// 
    /// ```
    /// use axum_insights::{AppInsights, Ready};
    /// use opentelemetry::runtime::Tokio;
    /// 
    /// let i: AppInsights<Ready> = AppInsights::default()
    ///     .with_connection_string(None)
    ///     .with_service_config("namespace", "name")
    ///     .with_runtime(Tokio);
    /// ```
    pub fn with_runtime<T>(self, runtime: T) -> AppInsights<Ready, C, T, P, E>
    where
        T: RuntimeChannel<BatchMessage>,
    {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
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
    pub fn with_catch_panic(self, should_catch_panic: bool) -> AppInsights<Ready, C, R, P, E> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
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
    pub fn with_field_mapper<F>(self, field_mapper: F) -> AppInsights<Ready, C, R, P, E>
    where
        F: Fn(&http::request::Parts) -> HashMap<String, String> + Send + Sync + 'static,
    {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: Some(Arc::new(field_mapper)),
            panic_mapper: self.panic_mapper,
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
    pub fn with_panic_mapper<F, T>(self, panic_mapper: F) -> AppInsights<Ready, C, R, T, E>
    where
        F: Fn(String) -> (u16, T) + Send + Sync + 'static,
    {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: Some(Arc::new(panic_mapper)),
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
    pub fn with_error_type<T>(self) -> AppInsights<Ready, C, R, P, T> {
        AppInsights {
            connection_string: self.connection_string,
            config: self.config,
            client: self.client,
            sample_rate: self.sample_rate,
            batch_runtime: self.batch_runtime,
            minimum_level: self.minimum_level,
            subscriber: self.subscriber,
            should_catch_panic: self.should_catch_panic,
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
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
    pub fn build_and_set_global_default(self) -> Result<AppInsightsComplete<P, E>, Box<dyn Error + Send + Sync + 'static>> {
        let Some(connection_string) = self.connection_string else {
            return Ok(AppInsightsComplete {
                field_mapper: self.field_mapper,
                panic_mapper: self.panic_mapper,
                _phantom: std::marker::PhantomData,
            });
        };

        let tracer = opentelemetry_application_insights::new_pipeline_from_connection_string(connection_string)?
            .with_client(self.client)
            .with_trace_config(self.config)
            .with_sample_rate(self.sample_rate)
            .install_batch(self.batch_runtime);

        let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

        let subscriber = self.subscriber.unwrap_or_default().with(telemetry).with(self.minimum_level);

        tracing::subscriber::set_global_default(subscriber)?;

        if self.should_catch_panic {
            let default_panic = panic::take_hook();

            panic::set_hook(Box::new(move |p| {
                let payload_string = format!("{:?}", p.payload().downcast_ref::<&str>());
                let backtrace = Backtrace::force_capture().to_string();

                // This doesn't work because this macro prescribes the name without allowing it to be overriden.
                tracing::error!(
                    ai.customEvent.name = "exception",
                    "exception.type" = "PANIC",
                    exception.message = payload_string,
                    exception.stacktrace = backtrace
                );

                default_panic(p);
            }));
        }

        Ok(AppInsightsComplete {
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
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
            field_mapper: self.field_mapper,
            panic_mapper: self.panic_mapper,
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
    field_mapper: OptionalFieldMapper,
    panic_mapper: OptionalPanicMapper<P>,
    _phantom: std::marker::PhantomData<E>,
}

impl<S, P, E> Layer<S> for AppInsightsLayer<P, E> {
    type Service = AppInsightsMiddleware<S, P, E>;

    fn layer(&self, inner: S) -> Self::Service {
        AppInsightsMiddleware {
            inner,
            field_mapper: self.field_mapper.clone(),
            panic_mapper: self.panic_mapper.clone(),
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
    field_mapper: OptionalFieldMapper,
    panic_mapper: OptionalPanicMapper<P>,
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
        let extra_fields = self.field_mapper.as_ref().map(|f| f(&parts)).unwrap_or(HashMap::new());

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
            otel.status_description = tracing::field::Empty,
            extra_fields = serde_json::to_string_pretty(&extra_fields).unwrap()
        );

        // Clone the panic mapper so that it can be used in the future.
        let panic_mapper = self.panic_mapper.clone();

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
                            .body(Body::from(error_string).boxed_unsync().map_err(axum::Error::new).boxed_unsync())
                            .unwrap())
                    }
                }?;

                // Get the response status information, and determine success.
                let status = response.status();
                let is_success = status.is_success() || status.is_redirection() || status.is_informational();

                // Get the span information about the response.
                let (response, otel_status, otel_status_description) = if is_success {
                    // The happy path!
                    (response, "OK", format!(r#"{{ "status": {} }}"#, status.as_u16()))
                } else {
                    // Extract the error from the response, so we can get some data for the response part of the span.

                    // Breakup the response into parts.
                    let (parts, body) = response.into_parts();

                    // Get the body bytes.
                    let body_bytes = hyper::body::to_bytes(body).await.unwrap_or(Bytes::new());

                    // Deserialize the error.
                    let error: E = serde_json::from_slice(&body_bytes).unwrap_or_default();

                    // Get the stringified error.
                    let error_string = serde_json::to_string_pretty(&error).unwrap();

                    // This doesn't work because this macro prescribes the name without allowing it to be overriden.
                    tracing::error!(
                        ai.customEvent.name = "exception",
                        "exception.type" = format!("HTTP {}", status.as_u16()),
                        exception.message = error.message().unwrap_or_default(),
                        exception.stacktrace = error.backtrace().unwrap_or_default()
                    );

                    // Recreate the body.
                    let body = Body::from(body_bytes).boxed_unsync().map_err(axum::Error::new).boxed_unsync();

                    // Recreate the response.
                    let response = Response::from_parts(parts, body);

                    (response, "ERROR", error_string)
                };

                // Finish the span.
                let span = Span::current().entered();

                span.record("http.response.status_code", status.as_u16());
                span.record("otel.status_code", otel_status);
                span.record("otel.status_description", otel_status_description);

                Ok(response)
            }
            .instrument(span),
        )
    }
}
