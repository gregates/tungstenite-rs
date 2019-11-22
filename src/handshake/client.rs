//! Client handshake machine.

use std::io::{Read, Write};
use std::marker::PhantomData;

use http::{HeaderMap, Request, Response, StatusCode};
use httparse::Status;
use log::*;

use super::headers::{FromHttparse, MAX_HEADERS};
use super::machine::{HandshakeMachine, StageResult, TryParse};
use super::{convert_key, HandshakeRole, MidHandshake, ProcessingResult};
use crate::error::{Error, Result};
use crate::protocol::{Role, WebSocket, WebSocketConfig};

/// Client handshake role.
#[derive(Debug)]
pub struct ClientHandshake<S> {
    verify_data: VerifyData,
    config: Option<WebSocketConfig>,
    _marker: PhantomData<S>,
}

impl<S: Read + Write> ClientHandshake<S> {
    /// Initiate a client handshake.
    pub fn start(
        stream: S,
        request: Request<()>,
        config: Option<WebSocketConfig>,
    ) -> Result<MidHandshake<Self>> {
        if request.method() != http::Method::GET {
            return Err(Error::Protocol(
                "Invalid HTTP method, only GET supported".into(),
            ));
        }

        if request.version() < http::Version::HTTP_11 {
            return Err(Error::Protocol(
                "HTTP version should be 1.1 or higher".into(),
            ));
        }

        // Check the URI scheme: only ws or wss are supported
        let _ = crate::client::uri_mode(request.uri())?;

        let key = generate_key();

        let machine = {
            let mut req = Vec::new();
            let uri = request.uri();
            write!(
                req,
                "\
                 GET {path} {version:?}\r\n\
                 Host: {host}\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: {key}\r\n",
                version = request.version(),
                host = uri
                    .host()
                    .ok_or_else(|| Error::Url("No host name in the URL".into()))?,
                path = uri
                    .path_and_query()
                    .ok_or_else(|| Error::Url("No path/query in URL".into()))?
                    .as_str(),
                key = key
            )
            .unwrap();
            for (k, v) in request.headers() {
                writeln!(req, "{}: {}\r", k, v.to_str()?).unwrap();
            }
            writeln!(req, "\r").unwrap();
            HandshakeMachine::start_write(stream, req)
        };

        let client = {
            let accept_key = convert_key(key.as_ref()).unwrap();
            ClientHandshake {
                verify_data: VerifyData { accept_key },
                config,
                _marker: PhantomData,
            }
        };

        trace!("Client handshake initiated.");
        Ok(MidHandshake {
            role: client,
            machine,
        })
    }
}

impl<S: Read + Write> HandshakeRole for ClientHandshake<S> {
    type IncomingData = Response<()>;
    type InternalStream = S;
    type FinalResult = (WebSocket<S>, Response<()>);
    fn stage_finished(
        &mut self,
        finish: StageResult<Self::IncomingData, Self::InternalStream>,
    ) -> Result<ProcessingResult<Self::InternalStream, Self::FinalResult>> {
        Ok(match finish {
            StageResult::DoneWriting(stream) => {
                ProcessingResult::Continue(HandshakeMachine::start_read(stream))
            }
            StageResult::DoneReading {
                stream,
                result,
                tail,
            } => {
                self.verify_data.verify_response(&result)?;
                debug!("Client handshake done.");
                let websocket =
                    WebSocket::from_partially_read(stream, tail, Role::Client, self.config);
                ProcessingResult::Done((websocket, result))
            }
        })
    }
}

/// Information for handshake verification.
#[derive(Debug)]
struct VerifyData {
    /// Accepted server key.
    accept_key: String,
}

impl VerifyData {
    pub fn verify_response(&self, response: &Response<()>) -> Result<()> {
        // 1. If the status code received from the server is not 101, the
        // client handles the response per HTTP [RFC2616] procedures. (RFC 6455)
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(Error::Http(response.status()));
        }
        let headers = response.headers();

        // 2. If the response lacks an |Upgrade| header field or the |Upgrade|
        // header field contains a value that is not an ASCII case-
        // insensitive match for the value "websocket", the client MUST
        // _Fail the WebSocket Connection_. (RFC 6455)
        if !headers
            .get("Upgrade")
            .and_then(|h| h.to_str().ok())
            .map(|h| h.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
        {
            return Err(Error::Protocol(
                "No \"Upgrade: websocket\" in server reply".into(),
            ));
        }
        // 3.  If the response lacks a |Connection| header field or the
        // |Connection| header field doesn't contain a token that is an
        // ASCII case-insensitive match for the value "Upgrade", the client
        // MUST _Fail the WebSocket Connection_. (RFC 6455)
        if !headers
            .get("Connection")
            .and_then(|h| h.to_str().ok())
            .map(|h| h.eq_ignore_ascii_case("Upgrade"))
            .unwrap_or(false)
        {
            return Err(Error::Protocol(
                "No \"Connection: upgrade\" in server reply".into(),
            ));
        }
        // 4.  If the response lacks a |Sec-WebSocket-Accept| header field or
        // the |Sec-WebSocket-Accept| contains a value other than the
        // base64-encoded SHA-1 of ... the client MUST _Fail the WebSocket
        // Connection_. (RFC 6455)
        if !headers
            .get("Sec-WebSocket-Accept")
            .map(|h| h == &self.accept_key)
            .unwrap_or(false)
        {
            return Err(Error::Protocol(
                "Key mismatch in Sec-WebSocket-Accept".into(),
            ));
        }
        // 5.  If the response includes a |Sec-WebSocket-Extensions| header
        // field and this header field indicates the use of an extension
        // that was not present in the client's handshake (the server has
        // indicated an extension not requested by the client), the client
        // MUST _Fail the WebSocket Connection_. (RFC 6455)
        // TODO

        // 6.  If the response includes a |Sec-WebSocket-Protocol| header field
        // and this header field indicates the use of a subprotocol that was
        // not present in the client's handshake (the server has indicated a
        // subprotocol not requested by the client), the client MUST _Fail
        // the WebSocket Connection_. (RFC 6455)
        // TODO

        Ok(())
    }
}

impl TryParse for Response<()> {
    fn try_parse(buf: &[u8]) -> Result<Option<(usize, Self)>> {
        let mut hbuffer = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Response::new(&mut hbuffer);
        Ok(match req.parse(buf)? {
            Status::Partial => None,
            Status::Complete(size) => Some((size, Response::from_httparse(req)?)),
        })
    }
}

impl<'h, 'b: 'h> FromHttparse<httparse::Response<'h, 'b>> for Response<()> {
    fn from_httparse(raw: httparse::Response<'h, 'b>) -> Result<Self> {
        if raw.version.expect("Bug: no HTTP version") < /*1.*/1 {
            return Err(Error::Protocol(
                "HTTP version should be 1.1 or higher".into(),
            ));
        }

        let headers = HeaderMap::from_httparse(raw.headers)?;

        let mut response = Response::new(());
        *response.status_mut() = StatusCode::from_u16(raw.code.expect("Bug: no HTTP status code"))?;
        *response.headers_mut() = headers;
        // TODO: httparse only supports HTTP 0.9/1.0/1.1 but not HTTP 2.0
        // so the only valid value we could get in the response would be 1.1.
        *response.version_mut() = http::Version::HTTP_11;

        Ok(response)
    }
}

/// Generate a random key for the `Sec-WebSocket-Key` header.
fn generate_key() -> String {
    // a base64-encoded (see Section 4 of [RFC4648]) value that,
    // when decoded, is 16 bytes in length (RFC 6455)
    let r: [u8; 16] = rand::random();
    base64::encode(&r)
}

#[cfg(test)]
mod tests {
    use super::super::machine::TryParse;
    use super::{generate_key, Response};

    #[test]
    fn random_keys() {
        let k1 = generate_key();
        println!("Generated random key 1: {}", k1);
        let k2 = generate_key();
        println!("Generated random key 2: {}", k2);
        assert_ne!(k1, k2);
        assert_eq!(k1.len(), k2.len());
        assert_eq!(k1.len(), 24);
        assert_eq!(k2.len(), 24);
        assert!(k1.ends_with("=="));
        assert!(k2.ends_with("=="));
        assert!(k1[..22].find("=").is_none());
        assert!(k2[..22].find("=").is_none());
    }

    #[test]
    fn response_parsing() {
        const DATA: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n";
        let (_, resp) = Response::try_parse(DATA).unwrap().unwrap();
        assert_eq!(resp.status(), http::StatusCode::OK);
        assert_eq!(
            resp.headers().get("Content-Type").unwrap(),
            &b"text/html"[..],
        );
    }
}
