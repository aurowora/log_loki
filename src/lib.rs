/*
Copyright (C) 2022 Aurora McGinnis

This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at http://mozilla.org/MPL/2.0/.
*/

use kanal::{unbounded, Sender};
use log::{set_boxed_logger, set_max_level, LevelFilter, Log, Metadata, Record, SetLoggerError};
#[cfg(feature = "tls")]
use rustls::client::ClientConfig;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::spawn;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

// background task for sending logs to loki
mod task;
use task::{LokiTask, LokiTaskMsg};
// Write logs in LogFmt style by default
mod fmt;
pub use fmt::LokiFormatter;
#[cfg(feature = "logfmt")]
mod logfmt;
#[cfg(feature = "logfmt")]
pub use logfmt::LogfmtFormatter;

/// `LokiBuilder` is used to construct the `Loki` object.
#[must_use = "Has no affect unless .build() is called."]
pub struct LokiBuilder {
    endpoint: Url,
    labels: HashMap<String, String>,
    headers: HashMap<String, String>,
    #[cfg(feature = "tls")]
    tls_config: Option<Arc<ClientConfig>>,
    max_log_lines: usize,
    max_log_lifetime: Duration,
    failure_policy: FailurePolicy,
    level_filter: LevelFilter,
    formatter: Option<Box<dyn LokiFormatter>>,
}

impl LokiBuilder {
    /// Construct a new Loki builder with the given endpoint and labels.
    pub fn new(endpoint: Url, labels: HashMap<String, String>) -> LokiBuilder {
        assert!(!labels.is_empty(), "At least one label must be specified!");

        LokiBuilder {
            endpoint,
            labels,
            headers: HashMap::new(),
            #[cfg(feature = "tls")]
            tls_config: None, // if unset, uses default
            max_log_lines: 4096,
            max_log_lifetime: Duration::from_secs(300),
            failure_policy: FailurePolicy::Retry(6),
            level_filter: LevelFilter::Trace,
            #[cfg(feature = "logfmt")]
            formatter: Some(Box::new(LogfmtFormatter::default())),
            #[cfg(not(feature = "logfmt"))]
            formatter: None,
        }
    }

    /// Specify a header to send in HTTP(s) requests to Loki.
    pub fn add_header(mut self, name: &str, value: &str) -> LokiBuilder {
        self.headers.insert(String::from(name), String::from(value));
        self
    }

    #[cfg(feature = "tls")]
    /// Configure rustls for HTTPS requests. Passed directly to ureq.
    pub fn tls_config(mut self, tls_config: Arc<ClientConfig>) -> LokiBuilder {
        self.tls_config = Some(tls_config);
        self
    }

    /// Specifies the maximum number of log lines that may be written before
    /// the log batch must be sent to Loki
    pub fn max_logs(mut self, lines: usize) -> LokiBuilder {
        self.max_log_lines = lines;
        self
    }

    /// Specifies the maximum number of seconds that log lines may resize in the buffer
    /// before they are sent to Loki
    pub fn max_log_lifetime(mut self, secs: Duration) -> LokiBuilder {
        self.max_log_lifetime = secs;
        self
    }

    /// Specifies how failures should be handled. The default is to retry up to 6 times.
    pub fn failure_policy(mut self, fp: FailurePolicy) -> LokiBuilder {
        self.failure_policy = fp;
        self
    }

    /// Sets the verbosity of this logger
    pub fn level(mut self, lf: LevelFilter) -> LokiBuilder {
        self.level_filter = lf;
        self
    }

    pub fn formatter(mut self, fmt: Box<dyn LokiFormatter>) -> LokiBuilder {
        self.formatter = Some(fmt);
        self
    }

    pub fn build(self) -> Loki {
        Loki::start(self)
    }
}

/// `FailurePolicy` specifies how failures should be handled.
#[derive(PartialEq, Debug, Clone, Eq)]
pub enum FailurePolicy {
    /// Log batches that fail to send are dropped
    Drop,
    /// Log batches that fail to send are retried up to the specified
    /// number of times on an exponential backoff curve up. Note that
    /// this only works if Loki accepts out of order writes.
    /// See: <https://grafana.com/docs/loki/latest/configuration/#accept-out-of-order-writes>
    Retry(usize),
}

/// Logger implementation that writes its logs to Loki. Create one using the `LokiBuilder`.
pub struct Loki {
    tx: Sender<LokiTaskMsg>,
    level_filter: LevelFilter,
    flush_notif: Arc<(Mutex<bool>, Condvar)>,
    fmt: Box<dyn LokiFormatter>,
}

impl Loki {
    fn start(b: LokiBuilder) -> Loki {
        let filter = b.level_filter;
        let (tx, rx) = unbounded::<LokiTaskMsg>();
        let flush_notif = Arc::new((Mutex::new(false), Condvar::new()));
        let flush_notif2 = Arc::clone(&flush_notif);
        let fmt = b.formatter;

        spawn(move || {
            #[cfg(feature = "tls")]
            LokiTask::new(
                rx,
                flush_notif2,
                b.endpoint,
                b.headers,
                b.labels,
                b.max_log_lines,
                b.max_log_lifetime,
                b.failure_policy,
                b.tls_config,
            )
            .run();
            #[cfg(not(feature = "tls"))]
            LokiTask::new(
                rx,
                flush_notif2,
                b.endpoint,
                b.headers,
                b.labels,
                b.max_log_lines,
                b.max_log_lifetime,
                b.failure_policy,
            )
            .run();
        });

        Loki {
            tx,
            level_filter: filter,
            flush_notif,
            fmt: fmt.expect(
                "When the logfmt feature is disabled, you are required to provide a formatter.",
            ),
        }
    }

    /// Installs the logger as the default logger for the entire program.
    /// Calling this (or any similar function from other libraries) more than once is a bug.
    pub fn apply(self) -> Result<(), SetLoggerError> {
        set_max_level(self.level_filter);
        set_boxed_logger(Box::from(self))
    }
}

impl Log for Loki {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level_filter
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("The current moment is after the Unix Epoch.")
            .as_nanos();

        let mut s = String::new();
        self.fmt
            .write_record(&mut s, record)
            .expect("LokiFormatters shouldn't fail here.");

        self.tx
            .send(LokiTaskMsg::Log(now, s))
            .expect("The other thread should be running.");
    }

    fn flush(&self) {
        let (mtx, cvar) = &*self.flush_notif;
        let mut flushed = mtx.lock().unwrap();

        self.tx
            .send(LokiTaskMsg::Flush)
            .expect("The other thread should be running");

        *flushed = false;

        while !*flushed {
            flushed = cvar.wait(flushed).unwrap();
        }
    }
}
