//! OTLP log export to OneUptime (QUI-1225).
//!
//! The faucet's `tracing` records are the only record that the chain's sudo key
//! was used to mint anything. Today they exist solely as Flux console output,
//! which is wiped on every update — not just on relocation — and cannot be
//! backfilled. This ships them to `observe.quip.network` as OTLP logs while
//! leaving the existing stdout stream exactly as it was.
//!
//! Configuration is read from the environment rather than from `Config`, because
//! the subscriber has to be built before `Config::parse()` runs. The names match
//! the two shell senders already in the estate (`bootnodes.quip.network` and
//! `quip-ipfs-node`, both `lib/telemetry.sh`), so all three are configured the
//! same way:
//!
//! | variable                   | source                        |
//! |----------------------------|-------------------------------|
//! | `TELEMETRY_ENDPOINT`       | baked into the image          |
//! | `TELEMETRY_SERVICE_NAME`   | baked into the image          |
//! | `ONEUPTIME_TELEMETRY_KEY`  | the Flux spec (Enterprise)    |
//! | `DISABLE_TELEMETRY`        | kill switch, no rebuild       |
//!
//! Telemetry is **off unless both the endpoint and the key are set**, so a local
//! `cargo run`, `cargo test` and CI are all silent no-ops.

use std::{collections::HashMap, env, panic::AssertUnwindSafe, time::Duration};

use opentelemetry::KeyValue;
use opentelemetry_otlp::{LogExporter, Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{logs::SdkLoggerProvider, Resource};

pub const ENDPOINT_VAR: &str = "TELEMETRY_ENDPOINT";
pub const SERVICE_NAME_VAR: &str = "TELEMETRY_SERVICE_NAME";
pub const KEY_VAR: &str = "ONEUPTIME_TELEMETRY_KEY";
pub const DISABLE_VAR: &str = "DISABLE_TELEMETRY";

/// OneUptime authenticates telemetry ingest on this header, never on a query
/// string — so the key cannot leak into an access log.
const AUTH_HEADER: &str = "x-oneuptime-token";

const DEFAULT_SERVICE_NAME: &str = "quipfaucet";

/// OTLP/HTTP signal path. See [`logs_url`] for why we append it ourselves.
const LOGS_PATH: &str = "/v1/logs";

/// Generous: the export runs on the batch processor's own thread, so a slow
/// endpoint delays telemetry and nothing else.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolved, validated telemetry settings.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    endpoint: String,
    key: String,
    service_name: String,
}

/// A live exporter. What it is pointed at is reported once, through
/// [`Startup::notice`], rather than being carried around unread.
pub struct Telemetry {
    provider: SdkLoggerProvider,
}

impl Telemetry {
    pub fn provider(&self) -> &SdkLoggerProvider {
        &self.provider
    }

    /// Flush and stop. Without this the last batch is lost on **every** redeploy,
    /// which is exactly when a record is most worth having.
    pub fn shutdown(&self) -> Result<(), String> {
        self.provider.shutdown().map_err(|err| err.to_string())
    }
}

/// Outcome of [`init`]. The notice is returned rather than logged because the
/// subscriber does not exist yet at the point this runs.
pub struct Startup {
    pub telemetry: Option<Telemetry>,
    pub notice: String,
}

/// Build the exporter, or explain why not. Never panics and never fails the
/// process: a faucet that cannot report is still a faucet that can dispense.
pub fn init() -> Startup {
    let settings = match resolve(
        env::var(ENDPOINT_VAR).ok(),
        env::var(KEY_VAR).ok(),
        env::var(SERVICE_NAME_VAR).ok(),
        env::var(DISABLE_VAR).ok().as_deref(),
    ) {
        Ok(settings) => settings,
        Err(reason) => {
            return Startup {
                telemetry: None,
                notice: format!("telemetry disabled: {reason}"),
            }
        }
    };

    // `build` can panic rather than return, and telemetry must never be able to
    // stop the faucet dispensing. opentelemetry-otlp constructs its HTTP client
    // on a spawned thread and joins it with `.unwrap()` — its own comment marks
    // that as a TODO — so a client that refuses to build arrives here as a panic
    // on this thread, not as an `Err`. Observed exactly once, locally: a missing
    // rustls provider. That is fixed at the root in `build`, and this keeps any
    // future instance of the same shape from taking the service with it.
    match std::panic::catch_unwind(AssertUnwindSafe(|| build(&settings))) {
        Ok(Ok(provider)) => {
            let notice = format!(
                "telemetry enabled: shipping logs to {} as {}",
                logs_url(&settings.endpoint),
                settings.service_name
            );
            Startup {
                telemetry: Some(Telemetry { provider }),
                notice,
            }
        }
        Ok(Err(err)) => Startup {
            telemetry: None,
            notice: format!("telemetry disabled: building the exporter failed: {err}"),
        },
        Err(_) => Startup {
            telemetry: None,
            notice: "telemetry disabled: the exporter panicked while starting; \
                     the faucet is serving without it (panic detail is on stderr)"
                .to_owned(),
        },
    }
}

/// Make `ring` the process-wide rustls provider.
///
/// reqwest is built with `rustls-no-provider` so that it reuses the provider
/// jsonrpsee already links, rather than dragging in a second one — but that
/// feature requires the default to be *installed*, and does not fall back to the
/// one compiled in. Without this, `reqwest::blocking::Client::builder().build()`
/// panics.
///
/// `ring` is the only provider in the graph (asserted by `cargo tree -i
/// aws-lc-rs` being empty), so this installs what rustls would otherwise have
/// selected implicitly and changes nothing for the faucet's WSS connection.
/// `install_default` errors only when a provider is already installed, which is
/// not a problem worth reporting.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn build(settings: &Settings) -> Result<SdkLoggerProvider, String> {
    install_crypto_provider();

    let mut headers = HashMap::new();
    headers.insert(AUTH_HEADER.to_owned(), settings.key.clone());

    let exporter = LogExporter::builder()
        .with_http()
        .with_endpoint(logs_url(&settings.endpoint))
        .with_protocol(Protocol::HttpBinary)
        .with_timeout(EXPORT_TIMEOUT)
        .with_headers(headers)
        .build()
        .map_err(|err| err.to_string())?;

    // `host.name` is deliberately constant rather than IP-derived. This app is
    // `instances: 1` and relocates; keying the host on its address would mint a
    // fresh host on every move and raise a phantom "host stopped reporting".
    // Same reasoning, and same choice, as quip-ipfs-node.
    //
    // `os.type` is required: without it, log rows never attach to the Host.
    // `container.runtime` is deliberately NOT sent — the exact values "docker"
    // and "podman" route the batch to a DockerHost row and kill the Hosts gauge.
    let resource = Resource::builder()
        .with_service_name(settings.service_name.clone())
        .with_attributes([
            KeyValue::new("host.name", settings.service_name.clone()),
            KeyValue::new("os.type", "linux"),
        ])
        .build();

    Ok(SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build())
}

/// Append the OTLP signal path to the configured base.
///
/// 🔴 This is not redundant, and the exporter will not do it for us.
/// `resolve_http_endpoint` in opentelemetry-otlp 0.32 uses a **programmatically
/// supplied** endpoint verbatim and appends `/v1/logs` only when the endpoint
/// came from `OTEL_EXPORTER_OTLP_ENDPOINT`. Passing the base straight through
/// would POST to `…/telemetry/otlp`, which 404s — the same failure that cost
/// quip-ipfs-node a round trip (`4666b14`, "it could not reach the endpoint at
/// all"), reached here by a different route.
///
/// `TELEMETRY_ENDPOINT` stays a *base* so it means the same thing in all three
/// senders; both shell implementations likewise post to `${endpoint}/v1/${sig}`.
fn logs_url(endpoint: &str) -> String {
    format!("{}{LOGS_PATH}", endpoint.trim_end_matches('/'))
}

/// Pure resolution, so the rules are testable without mutating process env.
fn resolve(
    endpoint: Option<String>,
    key: Option<String>,
    service_name: Option<String>,
    disable: Option<&str>,
) -> Result<Settings, &'static str> {
    if disable.is_some_and(is_truthy) {
        return Err("DISABLE_TELEMETRY is set");
    }

    let endpoint = non_empty(endpoint).ok_or("TELEMETRY_ENDPOINT is not set")?;
    // Fail closed on a plaintext endpoint: the ingestion key is a bearer
    // credential and must not cross the wire in the clear. Loopback is exempt so
    // the local sink used for testing still works.
    if !is_transport_acceptable(&endpoint) {
        return Err("TELEMETRY_ENDPOINT is plaintext http:// to a non-loopback host");
    }
    let key = non_empty(key).ok_or("ONEUPTIME_TELEMETRY_KEY is not set")?;
    let service_name = non_empty(service_name).unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned());

    Ok(Settings {
        endpoint,
        key,
        service_name,
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Matches clap's `FalseyValueParser`, which `--allow-any-chain` already uses, so
/// the two switches behave identically.
fn is_truthy(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "f" | "no" | "n" | "off"
    )
}

fn is_transport_acceptable(endpoint: &str) -> bool {
    if endpoint.starts_with("https://") {
        return true;
    }
    match endpoint.strip_prefix("http://") {
        // Bare host, `host:port`, or `host/path` — take the authority.
        Some(rest) => {
            let authority = rest.split('/').next().unwrap_or("");
            let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
            matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> (Option<String>, Option<String>, Option<String>) {
        (
            Some("https://observe.quip.network/telemetry/otlp".to_owned()),
            Some("11111111-2222-3333-4444-555555555555".to_owned()),
            Some("quipfaucet".to_owned()),
        )
    }

    #[test]
    fn logs_url_appends_the_signal_path() {
        assert_eq!(
            logs_url("https://observe.quip.network/telemetry/otlp"),
            "https://observe.quip.network/telemetry/otlp/v1/logs"
        );
    }

    #[test]
    fn logs_url_does_not_double_the_separator() {
        assert_eq!(
            logs_url("https://observe.quip.network/telemetry/otlp/"),
            "https://observe.quip.network/telemetry/otlp/v1/logs"
        );
    }

    #[test]
    fn logs_url_never_posts_to_the_bare_base() {
        // The whole point: a base must not survive unchanged into the exporter.
        let base = "https://observe.quip.network/telemetry/otlp";
        assert_ne!(logs_url(base), base);
        assert!(logs_url(base).ends_with("/v1/logs"));
    }

    #[test]
    fn resolves_when_endpoint_and_key_are_present() {
        let (endpoint, key, service) = settings();
        let resolved = resolve(endpoint, key, service, None).expect("should resolve");
        assert_eq!(resolved.service_name, "quipfaucet");
        assert_eq!(
            resolved.endpoint,
            "https://observe.quip.network/telemetry/otlp"
        );
    }

    #[test]
    fn service_name_falls_back_to_the_default() {
        let (endpoint, key, _) = settings();
        let resolved = resolve(endpoint, key, None, None).expect("should resolve");
        assert_eq!(resolved.service_name, DEFAULT_SERVICE_NAME);
    }

    #[test]
    fn off_without_a_key() {
        let (endpoint, _, service) = settings();
        assert!(resolve(endpoint, None, service, None).is_err());
    }

    #[test]
    fn off_without_an_endpoint() {
        let (_, key, service) = settings();
        assert!(resolve(None, key, service, None).is_err());
    }

    #[test]
    fn blank_values_count_as_unset() {
        let (endpoint, key, _) = settings();
        assert!(resolve(endpoint.clone(), Some("   ".to_owned()), None, None).is_err());
        assert!(resolve(Some(String::new()), key, None, None).is_err());
    }

    #[test]
    fn disable_switch_matches_clap_falsey_parsing() {
        let (endpoint, key, service) = settings();
        for off in ["", "0", "false", "F", "no", "off"] {
            assert!(
                resolve(endpoint.clone(), key.clone(), service.clone(), Some(off)).is_ok(),
                "{off:?} should NOT disable telemetry"
            );
        }
        for on in ["1", "true", "yes", "anything"] {
            assert!(
                resolve(endpoint.clone(), key.clone(), service.clone(), Some(on)).is_err(),
                "{on:?} should disable telemetry"
            );
        }
    }

    #[test]
    fn plaintext_endpoint_is_refused_except_on_loopback() {
        let (_, key, service) = settings();
        let refuse =
            |url: &str| resolve(Some(url.to_owned()), key.clone(), service.clone(), None).is_err();
        assert!(refuse("http://observe.quip.network/telemetry/otlp"));
        assert!(refuse("observe.quip.network/telemetry/otlp"));
        assert!(!refuse("http://127.0.0.1:4318"));
        assert!(!refuse("http://localhost:4318/telemetry/otlp"));
        assert!(!refuse("https://observe.quip.network/telemetry/otlp"));
    }
}
