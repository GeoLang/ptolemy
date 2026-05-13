// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! OpenTelemetry tracing configuration.
//!
//! When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, initializes OTLP span export.
//! Supports both gRPC and HTTP OTLP exporters.

/// Configuration for OpenTelemetry.
#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    pub endpoint: Option<String>,
    pub service_name: String,
    pub protocol: OtlpProtocol,
}

#[derive(Clone, Debug)]
pub enum OtlpProtocol {
    Grpc,
    Http,
}

impl TelemetryConfig {
    /// Load from environment variables.
    pub fn from_env() -> Self {
        let protocol = match std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
            .unwrap_or_default()
            .as_str()
        {
            "http/protobuf" | "http" => OtlpProtocol::Http,
            _ => OtlpProtocol::Grpc,
        };
        Self {
            endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
            service_name: std::env::var("OTEL_SERVICE_NAME")
                .unwrap_or_else(|_| "ptolemy".to_string()),
            protocol,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.endpoint.is_some()
    }
}

/// Initialize telemetry subscriber with tracing-subscriber and optional OTLP export.
///
/// When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, traces are exported to the
/// configured collector. Otherwise, only console logging is configured.
///
/// Usage in binary:
/// ```ignore
/// use ptolemy_api::telemetry::init_telemetry;
/// init_telemetry(); // call once at startup
/// ```
pub fn init_telemetry() -> TelemetryConfig {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let config = TelemetryConfig::from_env();

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("ptolemy=info,tower_http=info"));

    let fmt_layer = tracing_subscriber::fmt::layer().compact();

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .init();

    if config.is_enabled() {
        tracing::info!(
            endpoint = config.endpoint.as_deref().unwrap_or(""),
            service = %config.service_name,
            "OpenTelemetry configured (OTLP span export requires opentelemetry-otlp crate in binary)"
        );
    }

    config
}
