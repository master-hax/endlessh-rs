use std::collections::{HashMap, VecDeque};
use std::io::{copy, Cursor, Read, Write};
use std::io::ErrorKind;


use std::time::{Instant,Duration};

use httparse::Request;
use mio::Poll;
use mio::{Interest,{event, Token}};
use mio::net::{TcpListener,TcpStream};
#[cfg(unix)]
use mio::net::{UnixListener,UnixStream};

use httparse::Status;

const METRIC_HTTP_REQUEST_MAX_SIZE: usize = 8192;
const METRIC_CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const HTTP_404_RESPONSE: &str = "HTTP/1.1 404 Not Found\r\n\r\n";
const HTTP_405_RESPONSE: &str = "HTTP/1.1 405 Method Not Allowed\r\n\r\n";

type HttpRequestBuffer = [u8; METRIC_HTTP_REQUEST_MAX_SIZE];

enum MetricRequestStatus {
    ReadingRequest(HttpRequestBuffer, usize),
    WritingResponse(Box<dyn Read>),
}

struct HttpClient {
    stream: Box<dyn MioStream>,
    connected_time: Instant,
    connection_status: MetricRequestStatus,
}

trait MioStreamGiver: event::Source {
    // fn accept_stream(&self) -> std::io::Result<AcceptedMioStream>;
    fn accept_stream(&self) -> std::io::Result<Box<dyn MioStream>>;
}
impl MioStreamGiver for TcpListener {
    fn accept_stream(&self) -> std::io::Result<Box<dyn MioStream>> {
        let (stream,_addr) = self.accept()?;
        Ok(Box::new(stream))
    }
}

#[cfg(unix)]
impl MioStreamGiver for UnixListener {
    fn accept_stream(&self) -> std::io::Result<(Box<dyn MioStream>)> {
        let (stream,_addr) = self.accept()?;
        Ok((Box::new(stream)))
    }
}

trait MioStream: Read + Write + event::Source {}
impl MioStream for TcpStream {}
#[cfg(unix)]
impl MioStream for UnixStream {}

trait RequestHandler {
    fn handle_request() -> Box<dyn Read>;
}

fn generate_http_response(
    to_body: &impl ToString,
) -> String {
    let body = to_body.to_string();
    format!(
        concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/openmetrics-text; version=1.0.0; charset=utf-8\r\n",
            "Content-Length: {}\r\n\r\n{}",
        ),
        body.len(),
        body
    )
}

pub struct MetricServer {
    listener: Box<dyn MioStreamGiver>,
    listener_token: Token,
    listener_accept_available: bool,
    available_connections: VecDeque<Token>,
    current_connections: HashMap<Token,HttpClient>,
}



impl MetricServer {

    fn create(poll: &Poll, mut listener: Box<dyn MioStreamGiver>, listener_token: Token, client_token_range: std::ops::Range<usize>) -> Self {
        poll.registry().register(&mut listener, listener_token, Interest::READABLE).unwrap();
        let available_connections: VecDeque<Token> = client_token_range.into_iter()
        .map(|t| Token(t) )
        .collect();
        let num_available = available_connections.len();
        MetricServer {
            listener,
            listener_token,
            listener_accept_available: false,
            available_connections,
            current_connections: HashMap::with_capacity(num_available),
        }
    }

    pub fn new_tcp(poll: &Poll, listener: TcpListener, listener_token: Token, client_token_range: std::ops::Range<usize>) -> Self {
        MetricServer::create(poll, Box::new(listener), listener_token, client_token_range)
    }

    #[cfg(unix)]
    pub fn new_unix(poll: &Poll, mut listener: UnixListener, listener_token: Token, client_token_range: std::ops::Range<usize>) -> Self {
        MetricServer::create(poll, Box::new(listener), listener_token, client_token_range)
    }

    pub fn try_handle_event(&mut self, event: &event::Event, poll: &mut Poll, response: &impl ToString) -> bool {
        return if self.listener_token == event.token() {
            println!("metric server token");
            self.listener_accept_available = true;
            self.try_accept_new_connections(poll);
            true
        } else if let Some((client_token, client)) = self.current_connections.remove_entry(&event.token()) {
            println!("metric client token");
            if let Some(client) = self.handle_client(poll, &client_token, client, response) {
                assert!(self.current_connections.insert(client_token, client).is_none());
            } else {
                println!("available conn1: {:?}", self.available_connections);
                println!("killing client: {}", client_token.0);
                self.available_connections.push_back(client_token);
                println!("available conn: {:?}", self.available_connections);
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

            println!("doing an accept");
            println!("available tokens: {:?}", self.available_connections);

            // due to https://github.com/rust-lang/rust/issues/53667

            match self.listener.accept_stream() {
                Ok(mut stream) => {
                    
                    let token = self.available_connections.pop_front().expect("available connections is empty");
                    println!("accepting a new stream with token: {}", token.0);

                    poll.registry().register(&mut stream, token, Interest::READABLE).expect("failed to poll on metric client stream");
                    let new_client = HttpClient {
                        stream,
                        connected_time: Instant::now(),
                        connection_status: MetricRequestStatus::ReadingRequest([0_u8; METRIC_HTTP_REQUEST_MAX_SIZE], 0)
                    };
                    println!("accepted new metric connection with token {}", token.0);

                    self.current_connections.insert(token, new_client);
    
                },
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.listener_accept_available = false;
                }
                Err(e) => {
                    panic!("failed to accept connection: {}", e);
                }
            }
    
        }
    }

    fn handle_client(&self, poll: &mut Poll, token: &Token, mut client: HttpClient, http_response_body: &impl ToString) -> Option<HttpClient> {
        match client.connection_status {
        MetricRequestStatus::ReadingRequest(mut buffer, mut current_position) => {
            let cursor = &mut Cursor::new(&mut buffer[current_position..]);
            match copy(&mut client.stream, cursor) {
                Ok(0) => {
                    println!("copied no bytes to cursor");
                    poll.registry().deregister(&mut client.stream).unwrap();
                    return None;
                },
                Ok(n) => {
                    println!("read {} bytes to buffer", n);
                    current_position += cursor.position() as usize;
                },
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    println!("metric read would block");
                    current_position += cursor.position() as usize;
                },
                Err(e) => {
                    println!("cursor copy error: {}", e);
                    poll.registry().deregister(&mut client.stream).unwrap();
                    return None;
                },
            };
            let mut request_parser = Request::new(&mut []);
            match request_parser.parse( &buffer[..current_position] ) {
                Ok(Status::Complete(_)) | Err(httparse::Error::TooManyHeaders) => { },
                Ok(Status::Partial) => {
                    client.connection_status = MetricRequestStatus::ReadingRequest(buffer, current_position);
                    return Some(client);
                },
                Err(e) => {
                    println!("bad http request from metric client: {}", e);
                    poll.registry().deregister(&mut client.stream).unwrap();
                    return None;
                },
            };

            // http request has completed

            match request_parser.path {
                Some("/metrics") => {
                    match request_parser.method {
                        Some("GET") => {
                            let response = generate_http_response(http_response_body);
                            client.connection_status = MetricRequestStatus::WritingResponse(Box::new(Cursor::new(response)));
                        },
                        _ => {
                            client.connection_status = MetricRequestStatus::WritingResponse(Box::new(Cursor::new(HTTP_405_RESPONSE)));
                        },
                    }
                },
                _ => {
                    client.connection_status = MetricRequestStatus::WritingResponse(Box::new(Cursor::new(HTTP_404_RESPONSE)));
                },
            }
            poll.registry().reregister(&mut client.stream, *token, Interest::WRITABLE).unwrap();
            Some(client)
        },
        MetricRequestStatus::WritingResponse(mut to_write) => {
            match copy(&mut to_write, &mut client.stream) {
                Ok(0) => {
                    println!("wrote no bytes to client");
                },
                Ok(n) => {
                    println!("wrote {} bytes to client", n);
                },
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    println!("metric write would block");
                    client.connection_status = MetricRequestStatus::WritingResponse(to_write);
                    return Some(client)
                },
                Err(e) => {
                    println!("cursor copy error: {}", e);
                },
            };
            poll.registry().deregister(&mut client.stream).unwrap();
            None
        },
        }
    }
    
}