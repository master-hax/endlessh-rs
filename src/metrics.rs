use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::io::{copy, Cursor, Read};

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
    WritingResponse(Box<dyn Read>),
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
    ) -> Self {
        poll.registry()
            .register(&mut listener, listener_token, Interest::READABLE)
            .unwrap();
        let available_connections: VecDeque<Token> =
            client_token_range.into_iter().map(|t| Token(t)).collect();
        let num_available = available_connections.len();
        MetricServer {
            listener,
            listener_token,
            listener_accept_available: false,
            available_connections,
            current_connections: HashMap::with_capacity(num_available),
        }
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
        response: Box<dyn Read>,
    ) -> Option<HttpClient<Listener::Stream>> {
        if let Err(e) =
            poll.registry()
                .reregister(&mut client.stream, *token, Interest::WRITABLE)
        {
            eprintln!("failed to reregister metrics client: {}", e);
            return Self::close(poll, client);
        }
        client.connection_status = MetricRequestStatus::WritingResponse(response);
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
            MetricRequestStatus::ReadingRequest(buffer, current_position) => {
                // bounded read - can't overflow `buffer`, unlike `std::io::copy`
                match client.stream.read(&mut buffer[*current_position..]) {
                    Ok(0) => return Self::close(poll, client),
                    Ok(n) => {
                        *current_position += n;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_e) => return Self::close(poll, client),
                };
                let request_too_big = *current_position >= buffer.len();
                let mut headers = [httparse::EMPTY_HEADER; METRIC_HTTP_MAX_HEADERS];
                let mut request_parser = Request::new(&mut headers);
                let response: Box<dyn Read> = match request_parser.parse(&buffer[..*current_position])
                {
                    // request too big for our buffer, or has too many header fields
                    Ok(Status::Partial) if request_too_big => {
                        Box::new(Cursor::new(HTTP_431_RESPONSE))
                    }
                    Err(httparse::Error::TooManyHeaders) => {
                        Box::new(Cursor::new(HTTP_431_RESPONSE))
                    }
                    Ok(Status::Partial) => return Some(client),
                    Err(_e) => return Self::close(poll, client),
                    // http request has completed
                    Ok(Status::Complete(_)) => match request_parser.path {
                        Some("/metrics") => match request_parser.method {
                            Some("GET") => {
                                Box::new(Cursor::new(generate_http_response(http_response_body)))
                            }
                            _ => Box::new(Cursor::new(HTTP_405_RESPONSE)),
                        },
                        _ => Box::new(Cursor::new(HTTP_404_RESPONSE)),
                    },
                };
                Self::respond(poll, token, client, response)
            }
            MetricRequestStatus::WritingResponse(to_write) => {
                match copy(to_write, &mut client.stream) {
                    Err(e) if e.kind() == ErrorKind::WouldBlock => return Some(client),
                    _ => {}
                };
                if let Err(e) = poll.registry().deregister(&mut client.stream) {
                    eprintln!("failed to deregister metrics client: {}", e);
                }
                None
            }
        }
    }
}
