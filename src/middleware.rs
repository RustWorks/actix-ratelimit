use actix::dev::*;
use actix_web::{
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    error::Error as AWError,
    http::{HeaderName, HeaderValue},
    HttpResponse,
};
use futures::future::{ok, Ready};
use log::*;
use std::{
    cell::RefCell,
    future::Future,
    ops::Fn,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
};

use crate::{Messages, Responses};

/// Type that implements the ratelimit middleware. This accepts `interval` which specifies the
/// window size, `max_requests` which specifies the maximum number of requests in that window, and
/// `store` which is essentially a data store used to store client access information. Store is any
/// type that implements `RateLimit` trait.
pub struct RateLimiter<T>
where
    T: Handler<Messages> + Send + Sync + 'static,
    T::Context: ToEnvelope<T, Messages>,
{
    interval: Duration,
    max_requests: usize,
    store: Addr<T>,
    identifier: Rc<Box<dyn Fn(&ServiceRequest) -> String>>,
}

impl<T> RateLimiter<T>
where
    T: Handler<Messages> + Send + Sync + 'static,
    <T as Actor>::Context: ToEnvelope<T, Messages>,
{
    /// Creates a new instance of `RateLimiter`.
    pub fn new(store: Addr<T>) -> Self {
        let identifier = |req: &ServiceRequest| {
            let soc_addr = req.peer_addr().unwrap();
            soc_addr.ip().to_string()
        };
        RateLimiter {
            interval: Duration::from_secs(0),
            max_requests: 0,
            store: store,
            identifier: Rc::new(Box::new(identifier)),
        }
    }

    /// Specify the interval
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Specify the maximum number of requests allowed.
    pub fn with_max_requests(mut self, max_requests: usize) -> Self {
        self.max_requests = max_requests;
        self
    }
}

impl<T, S, B> Transform<S> for RateLimiter<T>
where
    T: Handler<Messages> + Send + Sync + 'static,
    T::Context: ToEnvelope<T, Messages>,
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = AWError> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = S::Error;
    type InitError = ();
    type Transform = RateLimitMiddleware<S, T>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(RateLimitMiddleware {
            service: Rc::new(RefCell::new(service)),
            store: self.store.clone(),
            max_requests: self.max_requests,
            interval: self.interval.as_secs(),
            identifier: self.identifier.clone(),
        })
    }
}

/// Middleware for RateLimiter.
pub struct RateLimitMiddleware<S, T>
where
    S: 'static,
    T: Handler<Messages> + 'static,
{
    service: Rc<RefCell<S>>,
    store: Addr<T>,
    // Exists here for the sole purpose of knowing the max_requests and interval from RateLimiter
    max_requests: usize,
    interval: u64,
    identifier: Rc<Box<dyn Fn(&ServiceRequest) -> String + 'static>>,
}

impl<T, S, B> Service for RateLimitMiddleware<S, T>
where
    T: Handler<Messages> + 'static,
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = AWError> + 'static,
    S::Future: 'static,
    B: 'static,
    T::Context: ToEnvelope<T, Messages>,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.borrow_mut().poll_ready(cx)
    }

    fn call(&mut self, req: ServiceRequest) -> Self::Future {
        let store = self.store.clone();
        let mut srv = self.service.clone();
        let max_requests = self.max_requests;
        let interval = Duration::from_secs(self.interval);
        let identifier = self.identifier.clone();
        Box::pin(async move {
            let identifier: String = (identifier)(&req);
            let remaining: Responses = store.send(Messages::Get(String::from(&identifier))).await?;
            match remaining {
                Responses::Get(opt) => {
                    let opt = opt.await?;
                    if let Some(c) = opt {
                        // Existing entry in store
                        let expiry = store
                            .send(Messages::Expire(String::from(&identifier)))
                            .await?;
                        let reset: Duration = match expiry {
                            Responses::Expire(dur) => dur.await?,
                            _ => unreachable!(),
                        };
                        if c == 0 {
                            info!("Limit exceeded for client: {}", &identifier);
                            let mut response = HttpResponse::TooManyRequests();
                            // let mut response = (error_callback)(&mut response);
                            response.set_header("x-ratelimit-limit", max_requests.to_string());
                            response.set_header("x-ratelimit-remaining", c.to_string());
                            response.set_header("x-ratelimit-reset", reset.as_secs().to_string());
                            Err(response.into())
                        } else {
                            // Decrement value
                            let res: Responses = store
                                .send(Messages::Update {
                                    key: identifier,
                                    value: 1,
                                })
                                .await?;
                            let updated_value: usize = match res {
                                Responses::Update(c) => c.await?,
                                _ => unreachable!(),
                            };
                            // Execute the request
                            let fut = srv.call(req);
                            let mut res = fut.await?;
                            let headers = res.headers_mut();
                            // Safe unwraps, since usize is always convertible to string
                            headers.insert(
                                HeaderName::from_static("x-ratelimit-limit"),
                                HeaderValue::from_str(max_requests.to_string().as_str())?,
                            );
                            headers.insert(
                                HeaderName::from_static("x-ratelimit-remaining"),
                                HeaderValue::from_str(updated_value.to_string().as_str())?,
                            );
                            headers.insert(
                                HeaderName::from_static("x-ratelimit-reset"),
                                HeaderValue::from_str(reset.as_secs().to_string().as_str())?,
                            );
                            Ok(res)
                        }
                    } else {
                        // New client, create entry in store
                        let current_value = max_requests - 1;
                        let res = store
                            .send(Messages::Set {
                                key: String::from(&identifier),
                                value: current_value,
                                expiry: interval,
                            })
                            .await?;
                        match res {
                            Responses::Set(c) => c.await?,
                            _ => unreachable!(),
                        }
                        let fut = srv.call(req);
                        let mut res = fut.await?;
                        let headers = res.headers_mut();
                        // Safe unwraps, since usize is always convertible to string
                        headers.insert(
                            HeaderName::from_static("x-ratelimit-limit"),
                            HeaderValue::from_str(max_requests.to_string().as_str()).unwrap(),
                        );
                        headers.insert(
                            HeaderName::from_static("x-ratelimit-remaining"),
                            HeaderValue::from_str(current_value.to_string().as_str()).unwrap(),
                        );
                        headers.insert(
                            HeaderName::from_static("x-ratelimit-reset"),
                            HeaderValue::from_str(interval.as_secs().to_string().as_str()).unwrap(),
                        );
                        Ok(res)
                    }
                }
                _ => {
                    unreachable!();
                }
            }
        })
    }
}
