use std::future::Future;
use std::pin::Pin;
use std::task;

use axum::extract::State as AxumState;
use futures::StreamExt;
use hyper::HeaderMap;
use pin_project_lite::pin_project;

use crate::connection::dump::exporter::export_dump;
use crate::error::Error;
use crate::namespace::MakeNamespace;

use super::db_factory::namespace_from_headers;
use super::AppState;

pin_project! {
    struct DumpStream<S> {
        join_handle: Option<tokio::task::JoinHandle<Result<(), Error>>>,
        #[pin]
        stream: S,
    }
}

impl<S> futures::Stream for DumpStream<S>
where
    S: futures::stream::TryStream + futures::stream::FusedStream,
    S::Error: Into<Error>,
{
    type Item = Result<S::Ok, Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        let this = self.as_mut().project();

        if !this.stream.is_terminated() {
            match futures::ready!(this.stream.try_poll_next(cx)) {
                Some(item) => task::Poll::Ready(Some(item.map_err(Into::into))),
                None => {
                    // poll join_handle
                    self.poll_next(cx)
                }
            }
        } else {
            // The stream was closed but we need to check if the dump task failed and forward the
            // error
            this.join_handle
                .take()
                .map_or(task::Poll::Ready(None), |mut join_handle| {
                    match Pin::new(&mut join_handle).poll(cx) {
                        task::Poll::Pending => {
                            *this.join_handle = Some(join_handle);
                            task::Poll::Pending
                        }
                        task::Poll::Ready(Ok(Err(err))) => task::Poll::Ready(Some(Err(err))),
                        task::Poll::Ready(Err(err)) => {
                            task::Poll::Ready(Some(Err(anyhow::anyhow!(err)
                                .context("Dump task crashed")
                                .into())))
                        }
                        task::Poll::Ready(Ok(Ok(_))) => task::Poll::Ready(None),
                    }
                })
        }
    }
}

pub(super) async fn handle_dump<F: MakeNamespace>(
    AxumState(state): AxumState<AppState<F>>,
    headers: HeaderMap,
) -> Result<axum::body::StreamBody<impl futures::Stream<Item = Result<bytes::Bytes, Error>>>, Error>
{
    let namespace = namespace_from_headers(
        &headers,
        state.disable_default_namespace,
        state.disable_namespaces,
    )?;

    let db_path = state
        .path
        .join("dbs")
        .join(std::str::from_utf8(namespace.as_ref()).expect("namespace to be a utf-8 string"))
        .join("data");

    let connection = rusqlite::Connection::open(db_path)?;

    let (reader, writer) = tokio::io::duplex(8 * 1024);

    let join_handle = tokio::task::spawn_blocking(move || {
        let writer = tokio_util::io::SyncIoBridge::new(writer);
        export_dump(connection, writer).map_err(Into::into)
    });

    let stream = tokio_util::io::ReaderStream::new(reader);

    let stream = DumpStream {
        stream: stream.fuse(),
        join_handle: Some(join_handle),
    };

    let stream = axum::body::StreamBody::new(stream);

    Ok(stream)
}
