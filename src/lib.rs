#![doc = include_str!("../README.md")]

#[cfg(test)]
mod tests;

pub mod errors;
pub mod governor;
pub mod key_extractor;
use crate::governor::{Governor, GovernorConfig};
use ::governor::clock::{Clock, DefaultClock, QuantaInstant};
use ::governor::middleware::{NoOpMiddleware, RateLimitingMiddleware, StateInformationMiddleware};

pub use errors::GovernorError;
use http::header::{HeaderName, HeaderValue};
use http::HeaderMap;
use hyper::body::Incoming;
use hyper::Request;
use hyper::Response;
use key_extractor::KeyExtractor;
use pin_project::pin_project;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin, task::ready};
use tower::{Layer, Service};
use jsonrpsee::http_client::HttpBody;

/// The Layer type that implements tower::Layer and is passed into `.layer()`
pub struct GovernorLayer<K, M>
where
    K: KeyExtractor,
    M: RateLimitingMiddleware<QuantaInstant>,
{
    pub config: Arc<GovernorConfig<K, M>>,
}

impl<K, M, S> Layer<S> for GovernorLayer<K, M>
where
    K: KeyExtractor,
    M: RateLimitingMiddleware<QuantaInstant>,
{
    type Service = Governor<K, M, S>;

    fn layer(&self, inner: S) -> Self::Service {
        Governor::new(inner, &self.config)
    }
}

/// https://stegosaurusdormant.com/understanding-derive-clone/
impl<K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> Clone for GovernorLayer<K, M> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
        }
    }
}
// Implement tower::Service for Governor
impl<K, S> Service<Request<Incoming>> for Governor<K, NoOpMiddleware, S>
where
    K: KeyExtractor,
    S: Service<Request<Incoming>, Response = Response<HttpBody>>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        if let Some(configured_methods) = &self.methods {
            if !configured_methods.contains(req.method()) {
                // The request method is not configured, we're ignoring this one.
                let future = self.inner.call(req);
                return ResponseFuture {
                    inner: Kind::Passthrough { future },
                };
            }
        }
        // Use the provided key extractor to extract the rate limiting key from the request.
        match self.key_extractor.extract(&req) {
            // Extraction worked, let's check if rate limiting is needed.
            Ok(key) => match self.limiter.check_key(&key) {
                Ok(_) => {
                    let future = self.inner.call(req);
                    ResponseFuture {
                        inner: Kind::Passthrough { future },
                    }
                }

                Err(negative) => {
                    let wait_time = negative
                        .wait_time_from(DefaultClock::default().now())
                        .as_secs();

                    #[cfg(feature = "tracing")]
                    {
                        let key_name = match self.key_extractor.key_name(&key) {
                            Some(n) => format!(" [{}]", &n),
                            None => "".to_owned(),
                        };
                        tracing::info!(
                            "Rate limit exceeded for {}{}, quota reset in {}s",
                            self.key_extractor.name(),
                            key_name,
                            &wait_time
                        );
                    }
                    
                    let body = HttpBody::from("Too many requests".to_string());
                    let response = Response::builder()
                      .status(429)      
                      .header("x-ratelimit-after", wait_time.to_string())        
                      .body(body)
                      .unwrap();

                    ResponseFuture {
                        inner: Kind::Error {
                            error_response: Some(response),
                        },
                    }
                }
            },

            Err(e) => {
                let body = HttpBody::from(e.to_string());
                let response = Response::builder()
                  .status(500)              
                  .body(body)
                  .unwrap();
                
              
                ResponseFuture {
                    inner: Kind::Error {
                        error_response: Some(response),
                    },
                }
            }
        }
    }
}

#[derive(Debug)]
#[pin_project]
/// Response future for [`Governor`].
pub struct ResponseFuture<F> {
    #[pin]
    inner: Kind<F>,
}

#[derive(Debug)]
#[pin_project(project = KindProj)]
enum Kind<F> {
    Passthrough {
        #[pin]
        future: F,
    },
    RateLimitHeader {
        #[pin]
        future: F,
        #[pin]
        burst_size: u32,
        #[pin]
        remaining_burst_capacity: u32,
    },
    WhitelistedHeader {
        #[pin]
        future: F,
    },
    Error {
        error_response: Option<Response<HttpBody>>
    },
}
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl<F, Error> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<HttpBody>, Error>>,
    Error: Into<BoxError>
{
    type Output = Result<Response<HttpBody>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().inner.project() {
            KindProj::Passthrough { future } => future.poll(cx),
            KindProj::RateLimitHeader {
                future,
                burst_size,
                remaining_burst_capacity,
            } => {
                let mut response = ready!(future.poll(cx))?;

                let mut headers = HeaderMap::new();
                headers.insert(
                    HeaderName::from_static("x-ratelimit-limit"),
                    HeaderValue::from(*burst_size),
                );
                headers.insert(
                    HeaderName::from_static("x-ratelimit-remaining"),
                    HeaderValue::from(*remaining_burst_capacity),
                );
                response.headers_mut().extend(headers.drain());
   

                Poll::Ready(Ok(response))
            }
            KindProj::WhitelistedHeader { future } => {
                let mut response = ready!(future.poll(cx))?;

                let headers = response.headers_mut();
                headers.insert(
                    HeaderName::from_static("x-ratelimit-whitelisted"),
                    HeaderValue::from_static("true"),
                );

                Poll::Ready(Ok(response))
            }
            KindProj::Error { error_response } => {
              let error = error_response.as_ref().unwrap();
              let body = HttpBody::from("Too many requests".to_string());
              let response = Response::builder()
                .status(error.status())              
                .body(body)
                .unwrap();
              
              Poll::Ready(Ok(response))
            },
        }
    }
}

// Implementation of Service for Governor using the StateInformationMiddleware.
impl<K, S> Service<Request<Incoming>> for Governor<K, StateInformationMiddleware, S>
where
    K: KeyExtractor,
    S: Service<Request<Incoming>, Response = Response<HttpBody>>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Our middleware doesn't care about backpressure so its ready as long
        // as the inner service is ready.
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        if let Some(configured_methods) = &self.methods {
            if !configured_methods.contains(req.method()) {
                // The request method is not configured, we're ignoring this one.
                let fut = self.inner.call(req);
                return ResponseFuture {
                    inner: Kind::WhitelistedHeader { future: fut },
                };
            }
        }
        // Use the provided key extractor to extract the rate limiting key from the request.
        match self.key_extractor.extract(&req) {
            // Extraction worked, let's check if rate limiting is needed.
            Ok(key) => match self.limiter.check_key(&key) {
                Ok(snapshot) => {
                    let fut = self.inner.call(req);
                    ResponseFuture {
                        inner: Kind::RateLimitHeader {
                            future: fut,
                            burst_size: snapshot.quota().burst_size().get(),
                            remaining_burst_capacity: snapshot.remaining_burst_capacity(),
                        },
                    }
                }

                Err(negative) => {
                    let wait_time = negative
                        .wait_time_from(DefaultClock::default().now())
                        .as_secs();

                    #[cfg(feature = "tracing")]
                    {
                        let key_name = match self.key_extractor.key_name(&key) {
                            Some(n) => format!(" [{}]", &n),
                            None => "".to_owned(),
                        };
                        tracing::info!(
                            "Rate limit exceeded for {}{}, quota reset in {}s",
                            self.key_extractor.name(),
                            key_name,
                            &wait_time
                        );
                    }


                    let body = HttpBody::from("Too many requests".to_string());
                    let response = Response::builder()
                      .status(429)      
                      .header("x-ratelimit-after", wait_time.to_string())
                      .header("x-ratelimit-limit", negative.quota().burst_size().get().to_string())
                      .header("x-ratelimit-remaining", "0")
                      .body(body)
                      .unwrap();

                    ResponseFuture {
                        inner: Kind::Error {
                            error_response: Some(response),
                        },
                    }
                }
            },

            // Extraction failed, stop right now.
            Err(e) => {
              let body = HttpBody::from(e.to_string());
              let response = Response::builder()
                .status(500)              
                .body(body)
                .unwrap();
              
                ResponseFuture {
                    inner: Kind::Error {
                        error_response: Some(response),
                    },
                }
            }
        }
    }
}
