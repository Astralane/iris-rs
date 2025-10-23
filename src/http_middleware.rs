use jsonrpsee::core::http_helpers::{Body, Request, Response};
use jsonrpsee::core::BoxError;
use metrics::{counter, histogram};
use tracing::debug;

#[derive(Clone)]
pub struct HttpLoggingMiddleware<S>(pub S);

impl<S> tower::Service<Request<Body>> for HttpLoggingMiddleware<S>
where
    S: tower::Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // Extract the X-Timestamp header
        let timestamp = req
            .headers()
            .get("X-Transaction-Timestamp")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());

        let origin_header = req
            .headers()
            .get("X-Client-IP")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        // Log the request details
        match timestamp {
            Some(ts) => {
                let now = chrono::Utc::now();
                let latency = now.signed_duration_since(ts.with_timezone(&chrono::Utc));
                match latency.num_microseconds() {
                    Some(latency_us) => {
                        histogram!("iris_http_request_receive_latency_us", "origin" => origin_header)
                            .record(latency_us as  f64);
                    }
                    None => {
                        counter!("iris_http_request_receive_latency_us", "origin" => origin_header)
                            .increment(1);
                    }
                }
            }
            None => {
                debug!("No X-Transaction-Timestamp header found in the request");
                counter!("iris_http_request_receive_latency_us_missing_timestamp", "origin" => origin_header)
                    .increment(1);
            }
        }

        // Pass the request to the inner service
        self.0.call(req)
    }
}
