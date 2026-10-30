/* unix signal handling for graceful shutdown (SIGTERM/SIGINT) and stats dumps (SIGUSR1) */

use std::fmt::Display;

use mio::{event, Interest, Poll, Token};
use mio_signals::{Signal, Signals};

use crate::EndlesshFatalError;

pub struct SignalHandler {
    token: Token,
    signals: Signals,
    should_terminate: bool,
    program_name: &'static str,
}

impl SignalHandler {
    pub fn new(
        poll: &Poll,
        token: Token,
        program_name: &'static str,
    ) -> Result<Self, EndlesshFatalError> {
        let mut signals = Signals::new(Signal::Terminate | Signal::Interrupt | Signal::User1)?;
        poll.registry()
            .register(&mut signals, token, Interest::READABLE)?;
        Ok(SignalHandler {
            token,
            signals,
            should_terminate: false,
            program_name,
        })
    }

    pub fn should_terminate(&self) -> bool {
        self.should_terminate
    }

    pub fn try_handle_event(&mut self, event: &event::Event, stats: &impl Display) -> bool {
        if event.token() != self.token {
            return false;
        }
        loop {
            match self.signals.receive() {
                Ok(Some(Signal::Terminate)) => {
                    eprintln!(
                        "{}: received SIGTERM, shutting down\n{}",
                        self.program_name, stats
                    );
                    self.should_terminate = true;
                    return true;
                }
                Ok(Some(Signal::Interrupt)) => {
                    eprintln!(
                        "{}: received SIGINT, shutting down\n{}",
                        self.program_name, stats
                    );
                    self.should_terminate = true;
                    return true;
                }
                Ok(Some(Signal::User1)) => eprintln!("{}", stats),
                Ok(Some(_)) => {}
                Ok(None) => return true,
                Err(e) => {
                    eprintln!("failed to read pending signal: {}", e);
                    return true;
                }
            }
        }
    }
}
