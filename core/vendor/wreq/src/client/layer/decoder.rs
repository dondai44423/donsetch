//! Middleware for decoding

use bytes::Buf;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use futures_util::{FutureExt, future::Map};
use http::{
    Request, Response,
    header::{self, HeaderValue},
};
use http_body::{Body, Frame, SizeHint};
use tower::{Layer, Service};
use tower_http::decompression::{self, DecompressionBody, ResponseFuture};

use crate::config::RequestConfig;

/// Configuration for supported content-encoding algorithms.
///
/// `AcceptEncoding` controls which compression formats are enabled for decoding
/// response bodies. Each field corresponds to a specific algorithm and is only
/// available if the corresponding feature is enabled.
#[derive(Clone)]
pub(crate) struct AcceptEncoding {
    #[cfg(feature = "gzip")]
    pub(crate) gzip: bool,
    #[cfg(feature = "brotli")]
    pub(crate) brotli: bool,
    #[cfg(feature = "zstd")]
    pub(crate) zstd: bool,
    #[cfg(feature = "deflate")]
    pub(crate) deflate: bool,
}

/// Layer that adds response body decompression to a service.
#[derive(Clone)]
pub struct DecompressionLayer {
    accept: AcceptEncoding,
}

/// Service that decompresses response bodies based on the [`AcceptEncoding`] configuration.
#[derive(Clone)]
pub struct Decompression<S>(Option<decompression::Decompression<PreserveContentMetadata<S>>>);

#[derive(Clone)]
struct PreserveContentMetadata<S>(S);

#[derive(Clone, Debug)]
pub(crate) struct OriginalContentMetadata {
    pub(crate) length: Option<HeaderValue>,
    pub(crate) encoding: Option<HeaderValue>,
    pub(crate) encoded_bytes: Arc<AtomicUsize>,
}

pin_project_lite::pin_project! {
    #[derive(Debug)]
    pub struct CountingBody<B> {
        #[pin]
        inner:B,
        encoded_bytes:Arc<AtomicUsize>,
    }
}

impl<B: Body> Body for CountingBody<B> {
    type Data = B::Data;
    type Error = B::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        match this.inner.poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.encoded_bytes
                        .fetch_add(data.remaining(), Ordering::Relaxed);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            other => other,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

type MetadataFuture<F, B, E> =
    Map<F, fn(Result<Response<B>, E>) -> Result<Response<CountingBody<B>>, E>>;

fn preserve_metadata<B, E>(result: Result<Response<B>, E>) -> Result<Response<CountingBody<B>>, E> {
    result.map(|mut response| {
        let encoded_bytes = Arc::new(AtomicUsize::new(0));
        let metadata = OriginalContentMetadata {
            length: response.headers().get(header::CONTENT_LENGTH).cloned(),
            encoding: response.headers().get(header::CONTENT_ENCODING).cloned(),
            encoded_bytes: Arc::clone(&encoded_bytes),
        };
        response.extensions_mut().insert(metadata);
        response.map(|inner| CountingBody {
            inner,
            encoded_bytes,
        })
    })
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for PreserveContentMetadata<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = Response<CountingBody<ResBody>>;
    type Error = S::Error;
    type Future = MetadataFuture<S::Future, ResBody, S::Error>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }
    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        self.0
            .call(request)
            .map(preserve_metadata::<ResBody, S::Error> as fn(_) -> _)
    }
}

// ===== AcceptEncoding =====

impl Default for AcceptEncoding {
    fn default() -> AcceptEncoding {
        AcceptEncoding {
            #[cfg(feature = "gzip")]
            gzip: true,
            #[cfg(feature = "brotli")]
            brotli: true,
            #[cfg(feature = "zstd")]
            zstd: true,
            #[cfg(feature = "deflate")]
            deflate: true,
        }
    }
}

impl_request_config_value!(AcceptEncoding);

// ===== impl DecompressionLayer =====

impl DecompressionLayer {
    /// Creates a new [`DecompressionLayer`] with the specified [`AcceptEncoding`].
    #[inline(always)]
    pub fn new(accept: AcceptEncoding) -> Self {
        Self { accept }
    }
}

impl<S> Layer<S> for DecompressionLayer {
    type Service = Decompression<S>;

    #[inline(always)]
    fn layer(&self, service: S) -> Self::Service {
        let decoder = decompression::Decompression::new(PreserveContentMetadata(service))
            .no_br()
            .no_deflate()
            .no_gzip()
            .no_zstd();
        Decompression(Some(Decompression::<S>::accept_in_place(
            decoder,
            &self.accept,
        )))
    }
}

// ===== impl Decompression =====

impl<S> Decompression<S> {
    const BUG_MSG: &str = "[BUG] Decompression service not initialized; bug in setup";

    fn accept_in_place(
        mut decoder: decompression::Decompression<PreserveContentMetadata<S>>,
        accept: &AcceptEncoding,
    ) -> decompression::Decompression<PreserveContentMetadata<S>> {
        #[cfg(feature = "gzip")]
        {
            decoder = decoder.gzip(accept.gzip);
        }

        #[cfg(feature = "deflate")]
        {
            decoder = decoder.deflate(accept.deflate);
        }

        #[cfg(feature = "brotli")]
        {
            decoder = decoder.br(accept.brotli);
        }

        #[cfg(feature = "zstd")]
        {
            decoder = decoder.zstd(accept.zstd);
        }

        decoder
    }
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for Decompression<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    ReqBody: Body,
    ResBody: Body,
{
    type Response = Response<DecompressionBody<CountingBody<ResBody>>>;
    type Error = S::Error;
    type Future = ResponseFuture<MetadataFuture<S::Future, ResBody, S::Error>>;

    #[inline(always)]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.as_mut().expect(Self::BUG_MSG).poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        if let Some(accept_encoding) = RequestConfig::<AcceptEncoding>::get(req.extensions()) {
            if let Some(decoder) = self.0.take() {
                self.0
                    .replace(Decompression::accept_in_place(decoder, accept_encoding));
            }
            debug_assert!(self.0.is_some());
        }

        self.0.as_mut().expect(Self::BUG_MSG).call(req)
    }
}
