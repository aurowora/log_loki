/*
Copyright (C) 2022 Aurora McGinnis

This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at http://mozilla.org/MPL/2.0/.
*/

use crate::FailurePolicy;
use core::cmp::Reverse;
use derivative::Derivative;
#[cfg(feature = "compress")]
use flate2::{write::GzEncoder, Compression};
use kanal::{ReceiveErrorTimeout, Receiver};
#[cfg(feature = "tls")]
use rustls::ClientConfig;
use serde::Serialize;
use serde_json::to_vec;
use std::collections::{BinaryHeap, HashMap};
#[cfg(feature = "compress")]
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ureq::{AgentBuilder, Error, Request};
use url::Url;

// LokiTask is a background thread that is used to send logs to Loki in the background
pub struct LokiTask {
    rx: Receiver<LokiTaskMsg>,
    request: Request,
    labels: HashMap<String, String>,
    max_log_lines: usize,
    max_log_lifetime: Duration,
    failure_policy: FailurePolicy,
    flush_notif: Arc<(Mutex<bool>, Condvar)>,
}

impl LokiTask {
    pub fn new(
        rx: Receiver<LokiTaskMsg>,
        flush_notif: Arc<(Mutex<bool>, Condvar)>,
        endpoint: Url,
        headers: HashMap<String, String>,
        labels: HashMap<String, String>,
        max_log_lines: usize,
        max_log_lifetime: Duration,
        failure_policy: FailurePolicy,
        #[cfg(feature = "tls")] tls_config: Option<Arc<ClientConfig>>,
    ) -> LokiTask {
        let mut agent_builder = AgentBuilder::new().timeout(Duration::from_secs(30));

        #[cfg(feature = "tls")]
        if let Some(tls_config) = tls_config {
            agent_builder = agent_builder.tls_config(tls_config);
        }

        let agent = agent_builder.build();
        let mut request = agent.request_url("POST", &endpoint);
        for (k, v) in headers {
            request = request.set(&k, &v);
        }
        request = request.set("Content-Type", "application/json; charset=utf-8");
        #[cfg(feature = "compress")]
        {
            request = request.set("Content-Encoding", "gzip");
        }

        LokiTask {
            rx,
            request,
            labels,
            max_log_lines,
            max_log_lifetime,
            failure_policy,
            flush_notif,
        }
    }

    // Thread loop.
    // Tries to receive messages from the channel, flushing before any limits are violated.
    // When not processing items from the channel, we'll retry failed items if there are any and check the age constraint.
    pub fn run(&self) {
        let mut lp = LokiPush {
            streams: [LokiStream {
                stream: self.labels.clone(),
                values: Vec::with_capacity(self.max_log_lines),
            }],
            first: None,
            failures: 0,
        };
        let mut dlq: BinaryHeap<Reverse<FailedPush>> = BinaryHeap::new();

        loop {
            loop {
                match self.rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(msg) => {
                        match msg {
                            LokiTaskMsg::Log(time, log_line) => {
                                lp.streams[0].values.push([format!("{}", time), log_line]);
                                if lp.streams[0].values.len() == self.max_log_lines {
                                    self.submit_logs(&mut lp, &mut dlq);
                                }
                                if lp.first.is_none() {
                                    lp.first = Some(time);
                                }
                            }
                            LokiTaskMsg::Flush => {
                                self.submit_logs(&mut lp, &mut dlq);
                                self.retry_all_failed(&mut dlq);

                                let (mtx, cvar) = &*self.flush_notif;
                                let mut flushed = mtx.lock().unwrap();
                                *flushed = true;
                                cvar.notify_all();
                            }
                        }
                        continue;
                    }
                    Err(ReceiveErrorTimeout::Timeout) => {
                        break;
                    }
                    // This matches Closed and SendClosed
                    Err(_) => {
                        self.submit_logs(&mut lp, &mut dlq);
                        self.retry_all_failed(&mut dlq);
                        return;
                    }
                }
            }

            if let Some(first_timestamp) = lp.first {
                let time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("The current moment is beyond the UNIX Epoch")
                    .as_nanos();

                if time - first_timestamp > self.max_log_lifetime.as_nanos() {
                    self.submit_logs(&mut lp, &mut dlq);
                    continue;
                }
            }

            while self.retry_failed(&mut dlq) {}
        }
    }

    // Send the push off to the server.
    fn submit_logs(&self, lp: &mut LokiPush, dlq: &mut BinaryHeap<Reverse<FailedPush>>) {
        if lp.first.is_none() {
            return;
        }

        // serialize json object
        #[allow(unused_mut)]
        let mut serialized = match to_vec(lp) {
            Ok(v) => v,
            Err(e) => {
                self.fail(lp, dlq, &e.to_string(), false);
                return;
            }
        };

        // perform gzip compression
        #[cfg(feature = "compress")]
        {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            match encoder.write_all(&serialized) {
                Ok(()) => match encoder.finish() {
                    Ok(w) => {
                        serialized = w;
                    }
                    Err(e) => {
                        self.fail(lp, dlq, &e.to_string(), false);
                        return;
                    }
                },
                Err(e) => {
                    self.fail(lp, dlq, &e.to_string(), false);
                    return;
                }
            }
        }

        // attempt to send the request
        let result = self.request.clone().send_bytes(&serialized);
        if result.is_err() {
            match result.expect_err("We already checked if the result was an error.") {
                Error::Status(code, resp) => {
                    self.fail(
                        lp,
                        dlq,
                        &format!("HTTP {}: {}", code, resp.status_text()),
                        code == 408 || code == 429 || code >= 500,
                    );
                    return;
                }
                e => {
                    self.fail(lp, dlq, &e.to_string(), true);
                    return;
                }
            }
        }

        // reset shared struct
        lp.streams[0].values.clear();
        lp.first = None;
    }

    // Handle failure of batch and optionally retry a transistent failure.
    fn fail(
        &self,
        lp: &mut LokiPush,
        dlq: &mut BinaryHeap<Reverse<FailedPush>>,
        emsg: &str,
        transistent: bool,
    ) {
        if self.failure_policy == FailurePolicy::Drop || !transistent {
            eprintln!(
                "(Loki) Failed to push batch of {} logs: {}; Dropping...",
                lp.streams[0].values.len(),
                emsg
            );
            return;
        } else if let FailurePolicy::Retry(max_retries) = self.failure_policy.clone() {
            if lp.failures > max_retries {
                eprintln!("(Loki) Failed to push batch of {} logs: {}; Exceeded max retries of {}, dropping...", lp.streams[0].values.len(), emsg, max_retries);
                return;
            }
            eprintln!(
                "(Loki) Failed to push batch of {} logs: {}; Attempt {} of {}",
                lp.streams[0].values.len(),
                emsg,
                lp.failures + 1,
                max_retries + 1
            );
        }
        let mut lpc = lp.clone();
        lpc.failures += 1;

        // reset shared struct
        lp.streams[0].values.clear();
        lp.first = None;

        // calculate backoff
        let retry_at: u128 = {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("The current moment is beyond the Unix Epoch.")
                .as_nanos()
        } + ((1 << lpc.failures) * 1_000_000_000); // exp backoff of 2^x

        dlq.push(Reverse(FailedPush {
            retry_at,
            push: Box::from(lpc),
        }));
    }

    // Retry a failed item if there is one to retry. Returns true if it did
    // something, false otherwise.
    fn retry_failed(&self, dlq: &mut BinaryHeap<Reverse<FailedPush>>) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("The current moment is beyond the Unix Epoch.")
            .as_nanos();

        if let Some(v) = dlq.peek() {
            if v.0.retry_at > now {
                return false;
            }
        } else {
            return false;
        }

        let mut lp = dlq
            .pop()
            .expect("We checked if this had a value in the peek() above")
            .0
            .push;
        self.submit_logs(&mut lp, dlq);
        true
    }

    // Retry everything during a forced flush.
    fn retry_all_failed(&self, dlq: &mut BinaryHeap<Reverse<FailedPush>>) {
        let mut t: BinaryHeap<Reverse<FailedPush>> = BinaryHeap::new();

        for v in dlq.drain() {
            self.submit_logs(&mut v.0.push.clone(), &mut t);
        }

        *dlq = t;
    }
}

// LokiTaskMsg is used by the main thread to send messages to the LokiTask
#[derive(Clone, Debug)]
pub enum LokiTaskMsg {
    Log(u128, String),
    Flush,
}

#[derive(Serialize, Clone)]
struct LokiPush {
    streams: [LokiStream; 1],
    #[serde(skip_serializing)]
    first: Option<u128>,
    #[serde(skip_serializing)]
    failures: usize,
}

#[derive(Serialize, Clone)]
struct LokiStream {
    stream: HashMap<String, String>,
    values: Vec<[String; 2]>,
}

#[derive(Derivative)]
#[derivative(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct FailedPush {
    retry_at: u128,
    #[derivative(PartialEq = "ignore", PartialOrd = "ignore", Ord = "ignore")]
    push: Box<LokiPush>,
}
