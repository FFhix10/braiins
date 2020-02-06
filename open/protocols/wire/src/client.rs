// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

use std::fmt::Debug;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use ii_async_compat::prelude::*;
use tokio::time;

use crate::Connection;
use crate::Framing;

/// Backoff generation for `ReConnection`.
pub trait Backoff: Debug {
    /// Called by `ReConnection` when next sleep duration is required.
    fn next(&mut self) -> Duration;

    /// Called by `ReConnection` when a connection is (re-)established
    /// so that the backoff type can eg. reset its state.
    fn reset(&mut self);
}

/// Default `Backoff` implementation, based on the fibonacci sequence.
#[derive(Debug)]
struct DefaultBackoff {
    current: u32,
    prev: u32,
    unit: Duration,
    max: Duration,
}

impl DefaultBackoff {
    /// Constructor. As `DefaultBackoff` produces numbers of the fibonacci sequence,
    /// each is multiplied by `unit` before being returned. In the `Default` implementation,
    /// the `unit` is 100 ms. This generates backoff of 100, 100, 200, 300, 500, ... milliseconds.
    ///
    /// `max` is the maximum backoff ever returned (after multiplication by `unit`). In the `Default`
    /// implementation this is 5 seconds.
    pub fn new(unit: Duration, max: Duration) -> Self {
        let mut res = DefaultBackoff {
            current: 0,
            prev: 0,
            unit,
            max,
        };

        res.reset();
        res
    }
}

impl Backoff for DefaultBackoff {
    fn next(&mut self) -> Duration {
        let current = self.current;
        let res = self.unit * current;

        if res >= self.max {
            self.max
        } else {
            let prev = self.prev;
            self.current = current + prev;
            self.prev = current;
            res
        }
    }

    fn reset(&mut self) {
        self.current = 1;
        self.prev = 0;
    }
}

impl Default for DefaultBackoff {
    fn default() -> Self {
        Self::new(Duration::from_millis(100), Duration::from_secs(5))
    }
}

/// The error type returned when a connection attempt fails.
///
/// The structure holds a few items related to backoff state
/// and the original connection error as defined by the `Framing` trait.
pub struct AttemptError<F: Framing> {
    /// Duration since the this (failed) till next time the `next()`
    /// will at the soonest perform another connection attempt.
    pub next_attempt_in: Duration,
    /// Number of failed reconnection attempts, including this one,
    /// since the connection broke.
    pub retries: u32,
    /// The instant when the first re-connection attempt was started after the connection broke.
    /// (You can use this to compute how long it has been in total since the connection broke
    /// by subtracting this from `Instant::now()`)
    pub start_time: Instant,
    /// The I/O error returned by the underlying `Connection`.
    pub error: F::Error,
}

impl<F: Framing> AttemptError<F> {
    fn new(next_attempt_in: Duration, retries: u32, start_time: Instant, error: F::Error) -> Self {
        Self {
            next_attempt_in,
            retries,
            start_time,
            error,
        }
    }
}

#[derive(Debug)]
pub struct Client<F: Framing> {
    /// Server address to connect to
    addr: SocketAddr,
    /// Backoff strategy trait object
    backoff: Box<dyn Backoff>,
    /// When connection attempt fails, current time (Instant) and a backoff Duration
    /// are saved here, this is used by next() to compute delay time before attempting
    /// connection when called next time.
    next_delay: Option<(Instant, Duration)>,
    /// Number of connection retries, reset when connection is established
    retries: u32,
    /// Time of the first attempt, reset if the connection is established,
    /// see AttemptError::start_time
    start_time: Option<Instant>,
    _marker: PhantomData<&'static F>,
}

impl<F: Framing> Client<F> {
    /// Create a new `Client` that will connect to `addr` with
    /// the default backoff.
    pub fn new(addr: SocketAddr) -> Self {
        Self::with_backoff(addr, DefaultBackoff::default())
    }

    /// Create a new `Client` that will connecto to `addr` with
    /// the supplied backoff.
    pub fn with_backoff<B: Backoff + 'static>(addr: SocketAddr, backoff: B) -> Self {
        Self {
            addr,
            backoff: Box::new(backoff),
            next_delay: None,
            retries: 0,
            start_time: None,
            _marker: PhantomData,
        }
    }

    pub fn set_addr(&mut self, addr: SocketAddr) {
        self.addr = addr;
    }

    pub fn set_backoff<B: Backoff + 'static>(&mut self, backoff: B) {
        self.backoff = Box::new(backoff);
    }

    pub async fn next(&mut self) -> Result<Connection<F>, AttemptError<F>> {
        self.start_time.get_or_insert(Instant::now());

        if let Some((when, delay)) = self.next_delay.take() {
            let since_last_attempt = Instant::now().duration_since(when);
            if delay > since_last_attempt {
                time::delay_for(delay - since_last_attempt).await;
            }
        }

        match Connection::connect(&self.addr).await {
            Ok(conn) => {
                self.backoff.reset();
                self.retries = 0;
                self.start_time = None;
                Ok(conn)
            }
            Err(err) => {
                let backoff = self.backoff.next();
                self.next_delay = Some((Instant::now(), backoff));
                self.retries += 1;
                let start_time = self.start_time.unwrap();
                Err(AttemptError::new(backoff, self.retries, start_time, err))
            }
        }
    }
}
