// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! OpenTelemetry tracing configuration.
//!
//! When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, initializes OTLP span export.
//! The actual subscriber setup is handled in the CLI binary.

/// Configuration for OpenTelemetry.
#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    pub endpoint: Option<String>,
    pub service_name: String,
}

impl TelemetryConfig {
    /// Load from environment variables.
    pub fn from_env() -> Self {
        Self {
            endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
            service_name: std::env::var("OTEL_SERVICE_NAME")
                .unwrap_or_else(|_| "ptolemy".to_string()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.endpoint.is_some()
    }
}

/// Initialize telemetry (placeholder — actual OTLP setup done at binary level).
pub fn init_telemetry() -> TelemetryConfig {
    let config = TelemetryConfig::from_env();
    if config.is_enabled() {
        tracing::info!(
            endpoint = config.endpoint.as_deref().unwrap_or(""),
            "OpenTelemetry configured"
        );
    }
    config
}
