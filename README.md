# log_loki

log_loki facilitates collecting and shipping your application's logs to a [Loki](https://grafana.com/oss/loki/) instance. It does this by integrating with the [log](https://docs.rs/log/latest/log/) crate.

Please be advised that I do not consider this crate production ready at this time. I've verified that it "basically works," but I have yet to write comphrensive tests and optimize out any inefficiencies that may exist. Caveat emptor!

## Installation

To add log_loki to your project, ensure the following two lines are present in your `Cargo.toml`:

```toml
log = "^0.4.17"
log_loki = "^0.1.0"
```

### Features

The crate supports the following features:
 - `tls` - Use rustls to support communicating with Loki over TLS.
 - `tls-native-certs` - Tell ureq, the underlying HTTP library, to use the system's certificate store instead of the webpki-roots store for TLS.
 - `compress` - Compress logs en route to Loki using GZIP (through the flate2 crate).
 - `kv_unstable` - Enable experimental support for the log crate's structured logging.
 - `logfmt` - Enable the logfmt formatter for logs.

 The default features are `tls`, `tls-native-certs`, `logfmt`, and `compress`. By default, the `logfmt` feature is used to format logs. If the feature is disabled, you must provide
 your own `LokiFormatter` implementation.

 ## Usage

 To log exclusively to an unauthenticated Loki instance, the following may be used:

 ```Rust
use log::{info, logger};
use log_loki::LokiBuilder;
use url::Url;
use std::collections::HashMap;

fn main() {
    let mut labels = HashMap::new();
    labels.insert("app", "myapp");

    LokiBuilder::new(
        Url::parse("https://loki.example.com/loki/api/v1/push").unwrap(),
        labels,
    ).build().apply().unwrap();

    info!("Hello, {}!", "world");

    // The logger must be flushed before the application quits to ensure logs are not lost.
    logger().flush();
}
 ```
Through the .add_header() and .tls_config() LokiBuilder methods, header and mTLS-based authentication schemes can be used.

If you'd like to log to Loki as well as other locations (such as a log file, console, etc), you can use a logging framework like Fern to combine log_loki with other logging implementations:

```Rust
// Let loki be a Loki object
let colors = ColoredLevelConfig::default();

fern::Dispatch::new()
    .level(log::LevelFilter::Trace)
    .chain(Box::new(loki) as Box<dyn log::Log>)
    .chain(fern::Dispatch::new()
         .format(move |out, message, record| {
            out.finish(format_args!("[{}] {}", colors.color(record.level()), message))
         })
        .chain(std::io::stdout())
        .into_shared()
    )
    .apply().unwrap();

info!("Test!");

// call somewhere before the program ends
logger().flush();
```
### Flushing

For efficiency's sake, the logger buffers log messages internally and waits until either a certain amount of messages have been logged or a certain amount of time has passed. You can tweek the number of messages
or the duration between auto-flushes using the `max_logs()` and `max_log_lifetime()` `LokiBuilder` methods respectively. It is also recommended that you arrange for all exit paths in your code to call `logger().flush();`
to minimize the risk of any logs being dropped.

## License

```Rust
/*
    Copyright (C) 2022 Aurora McGinnis

    This Source Code Form is subject to the terms of the Mozilla Public
    License, v. 2.0. If a copy of the MPL was not distributed with this
    file, You can obtain one at http://mozilla.org/MPL/2.0/.
*/
```