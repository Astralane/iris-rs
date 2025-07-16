use opentelemetry::trace::TracerProvider;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, WithExportConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::Resource;
use serde::Deserialize;
use tokio::runtime::Runtime;
use tracing::subscriber::set_global_default;
use tracing_log::LogTracer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

pub fn init_tracing<Sink>(endpoint: Option<String>, bind_port: u16, sink: Sink) -> Option<Runtime>
where
    Sink: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let service_name = format!(
        "iris_service_{}:{}",
        get_server_public_ip().unwrap_or(String::from("unknown_instance")),
        bind_port
    );
    match endpoint {
        Some(endpoint) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .thread_name("otel_runtime")
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("Failed to create Tokio runtime");
            // open telemetry calls tokio::spawn internally, so we need to enter the runtime context
            let _guard = rt.enter();
            let tracer = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(
                    opentelemetry_otlp::SpanExporter::builder()
                        .with_tonic()
                        .with_endpoint(endpoint.clone())
                        .build()
                        .expect("Couldn't create OTLP tracer"),
                )
                .with_resource(
                    Resource::builder()
                        .with_service_name(service_name.clone())
                        .build(),
                )
                .build()
                .tracer("iris");

            let telemetry_layer: tracing_opentelemetry::OpenTelemetryLayer<
                Registry,
                opentelemetry_sdk::trace::Tracer,
            > = tracing_opentelemetry::layer().with_tracer(tracer);

            let log_tracer = SdkLoggerProvider::builder()
                .with_batch_exporter(
                    LogExporter::builder()
                        .with_tonic()
                        .with_endpoint(endpoint)
                        .build()
                        .expect("Couldn't create OTL tracer"),
                )
                .with_resource(Resource::builder().with_service_name(service_name).build())
                .build();

            let logging_layer = OpenTelemetryTracingBridge::new(&log_tracer);

            let env_filter = EnvFilter::try_from_default_env().unwrap_or(EnvFilter::new("info"));
            let format_layer = fmt::Layer::default().with_writer(sink);

            let subscriber = Registry::default()
                .with(telemetry_layer)
                .with(logging_layer)
                .with(env_filter)
                .with(format_layer);
            LogTracer::init().expect("Failed to set log filter");
            set_global_default(subscriber).expect("Failed to set subscriber");
            Some(rt)
        }
        None => {
            let env_filter = EnvFilter::try_from_default_env().unwrap_or(EnvFilter::new("info"));
            let format_layer = fmt::Layer::default().with_writer(sink);
            let subscriber = Registry::default().with(format_layer).with(env_filter);
            LogTracer::init().expect("Failed to set log filter");
            set_global_default(subscriber).expect("Failed to set subscriber");
            None
        }
    }
}

#[derive(Deserialize)]
struct IpResponse {
    ip: String,
}

fn get_server_public_ip() -> Result<String, reqwest::Error> {
    let url = "https://api.ipify.org?format=json";
    let response = reqwest::blocking::get(url)?;
    let ip_data: IpResponse = response.json()?;
    Ok(ip_data.ip)
}
