use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::io::{Read, Write};

use httparse::Request;
use mio::Poll;
use mio::{
    Interest, {event, Token},
};

use httparse::Status;

use crate::listener::{MioListener, MioStream};

const METRIC_HTTP_REQUEST_MAX_SIZE: usize = 8192;
const METRIC_HTTP_MAX_HEADERS: usize = 32;

const HTTP_404_RESPONSE: &str = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";
const HTTP_405_RESPONSE: &str = "HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n";
const HTTP_431_RESPONSE: &str =
    "HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n";

type HttpRequestBuffer = [u8; METRIC_HTTP_REQUEST_MAX_SIZE];

enum MetricRequestStatus {
    ReadingRequest(HttpRequestBuffer, usize),
    WritingResponse(Cow<'static, [u8]>, usize),
}

struct HttpClient<Stream: MioStream> {
    stream: Stream,
    connection_status: MetricRequestStatus,
}

fn generate_http_response(to_body: &impl ToString) -> String {
    let body = to_body.to_string();
    format!(
        concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/openmetrics-text; version=1.0.0; charset=utf-8\r\n",
            "Connection: close\r\n",
            "Content-Length: {}\r\n\r\n{}",
        ),
        body.len(),
        body
    )
}

pub struct MetricServer<Listener: MioListener> {
    listener: Listener,
    listener_token: Token,
    listener_accept_available: bool,
    available_connections: VecDeque<Token>,
    current_connections: HashMap<Token, HttpClient<Listener::Stream>>,
}

impl<Listener: MioListener> MetricServer<Listener> {
    /// Creates a [`MetricServer`] that serves metrics requests on `listener`.
    pub fn new(
        poll: &Poll,
        mut listener: Listener,
        listener_token: Token,
        client_token_range: std::ops::Range<usize>,
    ) -> std::io::Result<Self> {
        poll.registry()
            .register(&mut listener, listener_token, Interest::READABLE)?;
        let available_connections: VecDeque<Token> =
            client_token_range.into_iter().map(|t| Token(t)).collect();
        let num_available = available_connections.len();
        Ok(MetricServer {
            listener,
            listener_token,
            listener_accept_available: false,
            available_connections,
            current_connections: HashMap::with_capacity(num_available),
        })
    }

    pub fn try_handle_event(
        &mut self,
        event: &event::Event,
        poll: &mut Poll,
        response: &impl ToString,
    ) -> bool {
        return if self.listener_token == event.token() {
            self.listener_accept_available = true;
            self.try_accept_new_connections(poll);
            true
        } else if let Some((client_token, client)) =
            self.current_connections.remove_entry(&event.token())
        {
            if let Some(client) = self.handle_client(poll, &client_token, client, response) {
                assert!(self
                    .current_connections
                    .insert(client_token, client)
                    .is_none());
            } else {
                self.available_connections.push_back(client_token);
            }
            // in case the number of clients dropped from the max
            self.try_accept_new_connections(poll);
            true
        } else {
            false
        };
    }

    fn try_accept_new_connections(&mut self, poll: &mut Poll) {
        while self.listener_accept_available && !self.available_connections.is_empty() {
            // due to https://github.com/rust-lang/rust/issues/53667

            match self.listener.accept_stream() {
                Ok(mut stream) => {
                    let token = self
                        .available_connections
                        .pop_front()
                        .expect("available connections is empty");

                    if let Err(e) =
                        poll.registry()
                            .register(&mut stream, token, Interest::READABLE)
                    {
                        eprintln!("failed to register metrics client: {}", e);
                        self.available_connections.push_back(token);
                        continue;
                    }
                    let new_client = HttpClient {
                        stream,
                        connection_status: MetricRequestStatus::ReadingRequest(
                            [0_u8; METRIC_HTTP_REQUEST_MAX_SIZE],
                            0,
                        ),
                    };

                    self.current_connections.insert(token, new_client);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.listener_accept_available = false;
                }
                Err(e) => {
                    // don't crash on a transient accept() failure - break instead of
                    // clearing listener_accept_available in case readiness is edge-triggered
                    eprintln!("failed to accept metrics connection: {}", e);
                    break;
                }
            }
        }
    }

    /// deregisters `client` and drops it, logging (rather than panicking) if deregistration fails.
    fn close(
        poll: &mut Poll,
        mut client: HttpClient<Listener::Stream>,
    ) -> Option<HttpClient<Listener::Stream>> {
        if let Err(e) = poll.registry().deregister(&mut client.stream) {
            eprintln!("failed to deregister metrics client: {}", e);
        }
        None
    }

    /// reregisters `client` for writing and queues `response`, closing it instead on failure.
    fn respond(
        poll: &mut Poll,
        token: &Token,
        mut client: HttpClient<Listener::Stream>,
        response: Cow<'static, [u8]>,
    ) -> Option<HttpClient<Listener::Stream>> {
        if let Err(e) =
            poll.registry()
                .reregister(&mut client.stream, *token, Interest::WRITABLE)
        {
            eprintln!("failed to reregister metrics client: {}", e);
            return Self::close(poll, client);
        }
        client.connection_status = MetricRequestStatus::WritingResponse(response, 0);
        Some(client)
    }

    fn handle_client(
        &self,
        poll: &mut Poll,
        token: &Token,
        mut client: HttpClient<Listener::Stream>,
        http_response_body: &impl ToString,
    ) -> Option<HttpClient<Listener::Stream>> {
        match &mut client.connection_status {
            MetricRequestStatus::ReadingRequest(request_buffer, current_position) => {
                // bounded read - can't overflow `request_buffer`, unlike `std::io::copy`
                match client.stream.read(&mut request_buffer[*current_position..]) {
                    Ok(0) => return Self::close(poll, client),
                    Ok(n) => {
                        *current_position += n;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_e) => return Self::close(poll, client),
                };
                let request_too_big = *current_position >= request_buffer.len();
                let mut headers = [httparse::EMPTY_HEADER; METRIC_HTTP_MAX_HEADERS];
                let mut request_parser = Request::new(&mut headers);
                let response: Cow<'static, [u8]> = match request_parser
                    .parse(&request_buffer[..*current_position])
                {
                    // request too big for our buffer, or has too many header fields
                    Ok(Status::Partial) if request_too_big => {
                        Cow::Borrowed(HTTP_431_RESPONSE.as_bytes())
                    }
                    Err(httparse::Error::TooManyHeaders) => {
                        Cow::Borrowed(HTTP_431_RESPONSE.as_bytes())
                    }
                    Ok(Status::Partial) => return Some(client),
                    Err(_e) => return Self::close(poll, client),
                    // http request has completed
                    Ok(Status::Complete(_)) => match request_parser.path {
                        Some("/metrics") => match request_parser.method {
                            Some("GET") => {
                                Cow::Owned(generate_http_response(http_response_body).into_bytes())
                            }
                            _ => Cow::Borrowed(HTTP_405_RESPONSE.as_bytes()),
                        },
                        _ => Cow::Borrowed(HTTP_404_RESPONSE.as_bytes()),
                    },
                };
                Self::respond(poll, token, client, response)
            }
            MetricRequestStatus::WritingResponse(response, written) => {
                while *written < response.len() {
                    match client.stream.write(&response[*written..]) {
                        Ok(0) => return Self::close(poll, client),
                        Ok(n) => *written += n,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => return Some(client),
                        Err(_e) => return Self::close(poll, client),
                    }
                }
                if let Err(e) = poll.registry().deregister(&mut client.stream) {
                    eprintln!("failed to deregister metrics client: {}", e);
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listener::HostDescription;
    use mio::Registry;
    use std::cell::{Cell, RefCell};
    use std::io::{self, Write};
    use std::rc::Rc;

    #[test]
    fn would_block_mid_write_does_not_lose_response_bytes() {
        #[derive(Clone)]
        struct MockStream {
            written: Rc<RefCell<Vec<u8>>>,
            write_call: Rc<Cell<usize>>,
            partial_n: usize,
        }

        impl Read for MockStream {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }

        impl Write for MockStream {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let call = self.write_call.get() + 1;
                self.write_call.set(call);
                match call {
                    // first write: socket only accepts part of the buffer
                    1 => {
                        let n = self.partial_n.min(buf.len());
                        self.written.borrow_mut().extend_from_slice(&buf[..n]);
                        Ok(n)
                    }
                    // second write: socket send buffer is now full
                    2 => Err(io::Error::from(ErrorKind::WouldBlock)),
                    // once polled again, the socket can accept the rest
                    _ => {
                        self.written.borrow_mut().extend_from_slice(buf);
                        Ok(buf.len())
                    }
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl event::Source for MockStream {
            fn register(&mut self, _r: &Registry, _t: Token, _i: Interest) -> io::Result<()> {
                Ok(())
            }
            fn reregister(&mut self, _r: &Registry, _t: Token, _i: Interest) -> io::Result<()> {
                Ok(())
            }
            fn deregister(&mut self, _r: &Registry) -> io::Result<()> {
                Ok(())
            }
        }

        impl MioStream for MockStream {
            fn host_description(&self) -> HostDescription {
                HostDescription::Unknown
            }
        }

        struct MockListener;

        impl event::Source for MockListener {
            fn register(&mut self, _r: &Registry, _t: Token, _i: Interest) -> io::Result<()> {
                Ok(())
            }
            fn reregister(&mut self, _r: &Registry, _t: Token, _i: Interest) -> io::Result<()> {
                Ok(())
            }
            fn deregister(&mut self, _r: &Registry) -> io::Result<()> {
                Ok(())
            }
        }

        impl MioListener for MockListener {
            type Stream = MockStream;
            fn accept_stream(&self) -> io::Result<MockStream> {
                unreachable!("test never accepts a connection")
            }
        }

        let mut poll = Poll::new().unwrap();
        let server = MetricServer::new(&poll, MockListener, Token(0), 1..2).unwrap();

        let body = "hello world, this is the full metrics response body".to_string();
        let written = Rc::new(RefCell::new(Vec::new()));
        let stream = MockStream {
            written: written.clone(),
            write_call: Rc::new(Cell::new(0)),
            partial_n: 5,
        };
        let client = HttpClient {
            stream,
            connection_status: MetricRequestStatus::WritingResponse(
                Cow::Owned(body.clone().into_bytes()),
                0,
            ),
        };
        let token = Token(2);

        let client = server
            .handle_client(&mut poll, &token, client, &"unused")
            .expect("connection should remain open to retry after WouldBlock");

        let client = server.handle_client(&mut poll, &token, client, &"unused");
        assert!(
            client.is_none(),
            "connection should close once the full response is sent"
        );

        assert_eq!(
            written.borrow().as_slice(),
            body.as_bytes(),
            "response body must not be truncated by a WouldBlock that occurs mid-write"
        );
    }
}
