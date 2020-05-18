//! `HttpService` server that uses Hyper as backend.

#![warn(rust_2018_idioms, missing_debug_implementations, missing_docs)]

use futures::{
    future::{BoxFuture, FutureExt as _, TryFutureExt as _},
    io::{AsyncRead, AsyncWrite},
    stream::{StreamExt as _, TryStream, TryStreamExt as _},
    task::Spawn,
};
use http_service::HttpService;
use hyper::{
    body::HttpBody as _,
    server::{Builder as HyperBuilder, Server as HyperServer},
};
use spawn_compat::SpawnCompat;
#[cfg(feature = "runtime")]
use std::net::SocketAddr;
use std::{
    convert::TryInto,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt as _};

mod spawn_compat;

struct WrapHttpService<H> {
    service: Arc<H>,
}

struct WrapConnection<H: HttpService> {
    service: Arc<H>,
    connection: H::Connection,
}

impl<'a, H, Conn> tower_service::Service<&'a Conn> for WrapHttpService<H>
where
    H: HttpService,
    H::ConnectionError: Into<http_service::Error> + Send + Sync + 'static,
{
    type Response = WrapConnection<H>;
    type Error = http_service::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: &'a Conn) -> Self::Future {
        let service = self.service.clone();
        let connection = service.connect().into_future();
        Box::pin(async move {
            let connection = connection.await.map_err(Into::into)?;
            Ok(WrapConnection {
                service,
                connection,
            })
        })
    }
}

impl<H> tower_service::Service<hyper::Request<hyper::Body>> for WrapConnection<H>
where
    H: HttpService,
    H::ConnectionError: Into<http_service::Error> + Send + Sync + 'static,
    H::ResponseError: Into<http_service::Error> + Send + Sync + 'static,
{
    type Response = hyper::Response<hyper::Body>;
    type Error = http_service::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: hyper::Request<hyper::Body>) -> Self::Future {
        fn convert(
            mut req: hyper::Request<hyper::Body>,
        ) -> Result<http_types::Request, http_service::Error> {
            let uri = std::mem::take(req.uri_mut());
            let mut parts = uri.into_parts();
            parts.scheme = Some(http::uri::Scheme::HTTP);
            parts.authority = Some(http::uri::Authority::from_static("localhost"));
            *req.uri_mut() = hyper::Uri::from_parts(parts)?;
            let (parts, body) = req.into_parts();
            let size = body
                .size_hint()
                .exact()
                .map(|size| size.try_into())
                .transpose()?;
            let reader = body
                .map(|res| res.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
                .into_async_read();
            let body = http_types::Body::from_reader(reader, size);
            let req = hyper::Request::from_parts(parts, body).try_into()?;
            Ok(req)
        }

        let res = convert(req).map(|req| {
            self.service
                .respond(self.connection.clone(), req)
                .into_future()
        });

        Box::pin(async move {
            let res = res?.await.map_err(Into::into)?;
            let res: hyper::Response<http_types::Body> = res.into();
            let (parts, body) = res.into_parts();
            let stream = futures_codec::FramedRead::new(body, futures_codec::BytesCodec);
            let body = hyper::Body::wrap_stream(stream);
            let res = hyper::Response::from_parts(parts, body);
            Ok(res)
        })
    }
}

pin_project_lite::pin_project! {
    struct WrapIncoming<I> {
        #[pin]
        incoming: I,
    }
}

impl<I: TryStream> hyper::server::accept::Accept for WrapIncoming<I>
where
    I::Ok: AsyncRead,
{
    type Conn = Compat<I::Ok>;
    type Error = I::Error;

    fn poll_accept(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Self::Conn, Self::Error>>> {
        self.project()
            .incoming
            .try_poll_next(cx)
            .map(|opt| opt.map(|res| res.map(|conn| conn.compat())))
    }
}

/// A listening HTTP server that accepts connections in both HTTP1 and HTTP2 by default.
///
/// [`Server`] is a [`Future`] mapping a bound listener with a set of service handlers. It is built
/// using the [`Builder`], and the future completes when the server has been shutdown. It should be
/// run by an executor.
#[allow(clippy::type_complexity)] // single-use type with many compat layers
pub struct Server<I: TryStream, S, Sp> {
    inner: HyperServer<WrapIncoming<I>, WrapHttpService<S>, SpawnCompat<Sp>>,
}

impl<I: TryStream, S, Sp> std::fmt::Debug for Server<I, S, Sp> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").finish()
    }
}

/// A builder for a [`Server`].
#[allow(clippy::type_complexity)] // single-use type with many compat layers
pub struct Builder<I: TryStream, Sp> {
    inner: HyperBuilder<WrapIncoming<I>, SpawnCompat<Sp>>,
}

impl<I: TryStream, Sp> std::fmt::Debug for Builder<I, Sp> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder").finish()
    }
}

impl<I: TryStream> Server<I, (), ()> {
    /// Starts a [`Builder`] with the provided incoming stream.
    pub fn builder(incoming: I) -> Builder<I, ()> {
        Builder {
            inner: HyperServer::builder(WrapIncoming { incoming }).executor(SpawnCompat::new(())),
        }
    }
}

impl<I: TryStream, Sp> Builder<I, Sp> {
    /// Sets the [`Spawn`] to deal with starting connection tasks.
    pub fn with_spawner<Sp2>(self, new_spawner: Sp2) -> Builder<I, Sp2> {
        Builder {
            inner: self.inner.executor(SpawnCompat::new(new_spawner)),
        }
    }

    /// Consume this [`Builder`], creating a [`Server`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use http_types::{Response, Body, StatusCode};
    /// use http_service_hyper::Server;
    /// use smol::Async;
    /// use std::net::TcpListener;
    ///
    /// // Construct an executor to run our tasks on
    /// #[derive(Debug, Clone)]
    /// struct SmolSpawner;
    ///
    /// impl futures::task::Spawn for SmolSpawner {
    ///     fn spawn_obj(
    ///         &self,
    ///         future: futures::future::FutureObj<'static, ()>
    ///     ) -> Result<(), futures::task::SpawnError> {
    ///         smol::Task::spawn(future).detach();
    ///         Ok(())
    ///     }
    ///
    ///     fn status(&self) -> Result<(), futures::task::SpawnError> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// // And an `HttpService` to handle each connection...
    /// let service = |req| async {
    ///     let mut res = Response::new(StatusCode::Ok);
    ///     res.set_body("Hello World");
    ///     Ok::<_, http_service::Error>(res)
    /// };
    ///
    /// // Then bind, configure the spawner to our pool, and serve...
    /// # let res: Result::<(), Box<dyn std::error::Error>> =
    /// smol::run(async move {
    ///     let mut listener = Async::<TcpListener>::bind("127.0.0.1:8080").unwrap();
    ///     let server = Server::builder(listener.incoming())
    ///         .with_spawner(SmolSpawner)
    ///         .serve(service)
    ///         .await?;
    ///     Ok(())
    /// })
    /// # ; res?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    pub fn serve<S: HttpService>(self, service: S) -> Server<I, S, Sp>
    where
        I: TryStream + Unpin,
        I::Ok: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        I::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
        S::ConnectionError: Into<http_service::Error> + Send + Sync + 'static,
        S::ResponseError: Into<http_service::Error> + Send + Sync + 'static,
        Sp: Clone + Spawn + Unpin + Send + 'static,
    {
        Server {
            inner: self.inner.serve(WrapHttpService {
                service: Arc::new(service),
            }),
        }
    }
}

impl<I, S, Sp> Future for Server<I, S, Sp>
where
    I: TryStream + Unpin,
    I::Ok: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    I::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S: HttpService,
    S::ConnectionError: Into<http_service::Error> + Send + Sync + 'static,
    S::ResponseError: Into<http_service::Error> + Send + Sync + 'static,
    Sp: Clone + Spawn + Unpin + Send + 'static,
{
    type Output = hyper::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<hyper::Result<()>> {
        self.inner.poll_unpin(cx)
    }
}

/// Serve the given `HttpService` at the given address, using `hyper` as backend, and return a
/// `Future` that can be `await`ed on.
#[cfg(feature = "runtime")]
pub fn serve<S: HttpService>(
    s: S,
    addr: SocketAddr,
) -> impl Future<Output = Result<(), hyper::Error>>
where
    S::ConnectionError: Into<http_service::Error> + Send + Sync + 'static,
    S::ResponseError: Into<http_service::Error> + Send + Sync + 'static,
{
    let service = WrapHttpService {
        service: Arc::new(s),
    };
    hyper::Server::bind(&addr).serve(service)
}
