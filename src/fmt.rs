/*
Copyright (C) 2022 Aurora McGinnis

This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at http://mozilla.org/MPL/2.0/.
*/

use log::Record;

/// `LokiFormatter` implementations marshals a log record to a string. This trait can be implemented
/// to customize the format of the strings that get sent to Loki. By default, this crate provides a
/// logfmt `LokiFormatter` implementation, which is used by default.
pub trait LokiFormatter: Send + Sync {
    fn write_record(&self, dst: &mut String, rec: &Record) -> std::fmt::Result;
}
