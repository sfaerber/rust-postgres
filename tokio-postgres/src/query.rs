use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::types::{BorrowToSql, IsNull};
use crate::{Error, Portal, Row, Statement};
use bytes::{Bytes, BytesMut};
use futures::{ready, Stream};
use log::{debug, log_enabled, Level};
use pin_project_lite::pin_project;
use postgres_protocol::message::backend::{Message, CommandCompleteBody};
use postgres_protocol::message::frontend;
use postgres_types::ProtocolEncodingFormat;
use smallvec::SmallVec;
use std::fmt;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::task::{Context, Poll};

struct BorrowToSqlParamsDebug<'a, T>(&'a [T]);

impl<'a, T> fmt::Debug for BorrowToSqlParamsDebug<'a, T>
where
    T: BorrowToSql,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|x| x.borrow_to_sql()))
            .finish()
    }
}

pub async fn query<P, I>(
    client: &InnerClient,
    statement: Statement,
    params: I,
    result_formats: &[ProtocolEncodingFormat],
) -> Result<RowStream, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let buf = if log_enabled!(Level::Debug) {
        let params = params.into_iter().collect::<Vec<_>>();
        debug!(
            "executing statement {} with parameters: {:?}",
            statement.name(),
            BorrowToSqlParamsDebug(params.as_slice()),
        );
        encode(client, &statement, params, result_formats)?
    } else {
        encode(client, &statement, params, result_formats)?
    };
    let responses = start(client, buf).await?;
    Ok(RowStream {
        statement,
        responses,
        _p: PhantomPinned,
        extract_allowed: result_formats
            .into_iter()
            .all(|e| ProtocolEncodingFormat::Binary == *e),
    })
}

pub async fn query_portal(
    client: &InnerClient,
    portal: &Portal,
    max_rows: i32,
    result_formats: &[ProtocolEncodingFormat],
) -> Result<RowStream, Error> {
    let buf = client.with_buf(|buf| {
        frontend::execute(portal.name(), max_rows, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })?;

    let responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    Ok(RowStream {
        statement: portal.statement().clone(),
        responses,
        _p: PhantomPinned,
        extract_allowed: result_formats
            .into_iter()
            .all(|e| ProtocolEncodingFormat::Binary == *e),
    })
}

pub async fn execute<P, I>(
    client: &InnerClient,
    statement: Statement,
    params: I,
) -> Result<Option<CommandCompleteBody>, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let buf = if log_enabled!(Level::Debug) {
        let params = params.into_iter().collect::<Vec<_>>();
        debug!(
            "executing statement {} with parameters: {:?}",
            statement.name(),
            BorrowToSqlParamsDebug(params.as_slice()),
        );
        encode(
            client,
            &statement,
            params,
            &[ProtocolEncodingFormat::Binary],
        )?
    } else {
        encode(
            client,
            &statement,
            params,
            &[ProtocolEncodingFormat::Binary],
        )?
    };
    let mut responses = start(client, buf).await?;

    let mut rows = None;
    loop {
        match responses.next().await? {
            Message::DataRow(_) => {}
            Message::CommandComplete(body) => {
                rows = Some(body);
            }
            Message::EmptyQueryResponse => { },
            Message::ReadyForQuery(_) => return Ok(rows),
            _ => return Err(Error::unexpected_message()),
        }
    }
}

async fn start(client: &InnerClient, buf: Bytes) -> Result<Responses, Error> {
    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    match responses.next().await? {
        Message::BindComplete => {}
        _ => return Err(Error::unexpected_message()),
    }

    Ok(responses)
}

pub fn encode<P, I>(
    client: &InnerClient,
    statement: &Statement,
    params: I,
    result_formats: &[ProtocolEncodingFormat],
) -> Result<Bytes, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    client.with_buf(|buf| {
        encode_bind(statement, params, "", buf, result_formats)?;
        frontend::execute("", 0, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })
}

pub fn encode_bind<P, I>(
    statement: &Statement,
    params: I,
    portal: &str,
    buf: &mut BytesMut,
    result_formats: &[ProtocolEncodingFormat],
) -> Result<(), Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let params: SmallVec<[_; 4]> = params.into_iter().collect();

    assert!(
        statement.params().len() == params.len(),
        "expected {} parameters but got {}",
        statement.params().len(),
        params.len()
    );

    let mut error_idx = 0;
    let r = frontend::bind(
        portal,
        statement.name(),
        params
            .iter()
            .map(|p| p.borrow_to_sql().parameter_encoding_format().into())
            .collect::<SmallVec<[_; 8]>>(),
        params.into_iter().zip(statement.params()).enumerate(),
        |(idx, (param, ty)), buf| match param.borrow_to_sql().to_sql_checked(ty, buf) {
            Ok(IsNull::No) => Ok(postgres_protocol::IsNull::No),
            Ok(IsNull::Yes) => Ok(postgres_protocol::IsNull::Yes),
            Err(e) => {
                error_idx = idx;
                Err(e)
            }
        },
        result_formats.into_iter().map(|e| (*e).into()),
        buf,
    );
    match r {
        Ok(()) => Ok(()),
        Err(frontend::BindError::Conversion(e)) => Err(Error::to_sql(e, error_idx)),
        Err(frontend::BindError::Serialization(e)) => Err(Error::encode(e)),
    }
}

pin_project! {
    /// A stream of table rows.
    pub struct RowStream {
        statement: Statement,
        responses: Responses,
        #[pin]
        _p: PhantomPinned,
        extract_allowed: bool,
    }
}

impl Stream for RowStream {
    type Item = Result<Row, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        loop {
            match ready!(this.responses.poll_next(cx)?) {
                Message::DataRow(body) => {
                    return Poll::Ready(Some(Ok(Row::new(
                        this.statement.clone(),
                        body,
                        *this.extract_allowed,
                    )?)))
                }
                Message::EmptyQueryResponse
                | Message::CommandComplete(_)
                | Message::PortalSuspended => {}
                Message::ReadyForQuery(_) => return Poll::Ready(None),
                _ => return Poll::Ready(Some(Err(Error::unexpected_message()))),
            }
        }
    }
}
