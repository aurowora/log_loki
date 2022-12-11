/*
Copyright (C) 2022 Aurora McGinnis

This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at http://mozilla.org/MPL/2.0/.
*/

use crate::LokiFormatter;
use bitflags::bitflags;
#[cfg(feature = "kv_unstable")]
use log::kv::{value::Error as LogError, Key, Value, Visitor};
use log::Record;
use std::collections::HashSet;
use std::fmt::Write;

// Contains all characters that may not appear in logfmt keys
const INVALID_KEY_CHARS: &[char] = &[' ', '=', '"'];

/// `LogfmtFormatter` provides a `LokiFormatter` that marshals logs using the logfmt format, which is a
/// plain text log format that is easy for both humans and machines to read and write. Loki provides
/// support for logfmt out of the box. This is used as the default formatter for the Loki logger if
/// the `logfmt` feature is enabled.
/// To learn more about logfmt, see: <https://www.brandur.org/logfmt>
#[derive(Default, Debug)]
pub struct LogfmtFormatter {
    include_fields: LogfmtAutoFields,
    escape_newlines: bool,
}

impl LogfmtFormatter {
    /// Create a new `LogfmtFormatter`. The created formatter will automatically insert fields
    /// depending on the value of include_fields. See `LogfmtAutoFields` for more details.
    /// \r, \n, and \t can be optionally escaped depending on the value of escape_newlines, but
    /// Loki does not require this.
    pub fn new(include_fields: LogfmtAutoFields, escape_newlines: bool) -> Self {
        LogfmtFormatter {
            include_fields,
            escape_newlines,
        }
    }

    /// Write a key value pair to the underlying string. Duplicate keys are dropped.
    fn write_pair(
        &self,
        dst: &mut String,
        used_fields: &mut HashSet<String>,
        key: &mut String,
        val: &str,
    ) -> std::fmt::Result {
        // Normalize the key
        key.retain(|c| {
            for invalid_char in INVALID_KEY_CHARS {
                if c == *invalid_char {
                    return false;
                }
            }
            true
        });
        if key.is_empty() {
            key.push('_');
        }

        // ensure uniqueness of the key
        if used_fields.contains(key) {
            return Ok(());
        }
        used_fields.insert(key.clone());

        // reformat the value if needed
        let mut formatted_value = String::new();
        formatted_value.reserve(val.len() + 10);
        let mut need_quotes = false;
        for chr in val.chars() {
            match chr {
                '\\' | '"' => {
                    need_quotes = true;
                    formatted_value.push('\\');
                    formatted_value.push(chr);
                }
                ' ' | '=' => {
                    need_quotes = true;
                    formatted_value.push(chr);
                }

                '\n' | '\r' | '\t' => {
                    need_quotes = true;

                    if self.escape_newlines {
                        formatted_value.push('\\');
                    }

                    formatted_value.push(chr);
                }
                _ => {
                    if !chr.is_control() {
                        formatted_value.push(chr);
                    } else {
                        need_quotes = true;
                        formatted_value.push_str(&chr.escape_unicode().to_string());
                    }
                }
            }
        }
        if need_quotes {
            formatted_value.push('"');
        }

        write!(
            dst,
            "{}{}={}{}",
            {
                if used_fields.is_empty() {
                    ""
                } else {
                    " "
                }
            },
            key,
            {
                if need_quotes {
                    "\""
                } else {
                    ""
                }
            },
            formatted_value
        )
    }
}

impl LokiFormatter for LogfmtFormatter {
    fn write_record(&self, dst: &mut String, rec: &Record) -> std::fmt::Result {
        let mut used_fields: HashSet<String> = HashSet::new();
        used_fields.reserve(10);

        if self.include_fields.contains(LogfmtAutoFields::LEVEL) {
            self.write_pair(
                dst,
                &mut used_fields,
                &mut "level".to_owned(),
                &rec.level().to_string().to_lowercase(),
            )?;
        }

        if self.include_fields.contains(LogfmtAutoFields::MESSAGE) && rec.args().to_string() != "" {
            self.write_pair(
                dst,
                &mut used_fields,
                &mut "message".to_owned(),
                &rec.args().to_string(),
            )?;
        }

        if self.include_fields.contains(LogfmtAutoFields::TARGET) && rec.target() != "" {
            self.write_pair(
                dst,
                &mut used_fields,
                &mut "target".to_owned(),
                rec.target(),
            )?;
        }

        if self.include_fields.contains(LogfmtAutoFields::MODULE_PATH) {
            let module = {
                if rec.module_path().is_some() {
                    rec.module_path()
                } else if rec.module_path_static().is_some() {
                    rec.module_path_static()
                } else {
                    None
                }
            };

            if let Some(m) = module {
                self.write_pair(dst, &mut used_fields, &mut "module".to_owned(), m)?;
            }
        }

        if self.include_fields.contains(LogfmtAutoFields::FILE) {
            let file = {
                if rec.file().is_some() {
                    rec.file()
                } else if rec.file_static().is_some() {
                    rec.file_static()
                } else {
                    None
                }
            };

            if let Some(f) = file {
                self.write_pair(dst, &mut used_fields, &mut "file".to_owned(), f)?;
            }
        }

        if self.include_fields.contains(LogfmtAutoFields::LINE) && rec.line().is_some() {
            self.write_pair(
                dst,
                &mut used_fields,
                &mut "line".to_owned(),
                &rec.line().unwrap().to_string(),
            )?;
        }

        #[cfg(feature = "kv_unstable")]
        if self.include_fields.contains(LogfmtAutoFields::EXTRA) {
            rec.key_values()
                .visit(&mut LogfmtVisitor {
                    dst,
                    fmt: self,
                    used: &mut used_fields,
                })
                .expect("This visitor should not return an error");
        }

        Ok(())
    }
}

#[cfg(feature = "kv_unstable")]
struct LogfmtVisitor<'a> {
    dst: &'a mut String,
    fmt: &'a LogfmtFormatter,
    used: &'a mut HashSet<String>,
}

#[cfg(feature = "kv_unstable")]
impl<'a, 'kvs> Visitor<'kvs> for LogfmtVisitor<'a> {
    fn visit_pair(&mut self, key: Key<'kvs>, value: Value<'kvs>) -> Result<(), LogError> {
        self.fmt.write_pair(
            self.dst,
            self.used,
            &mut key.to_string(),
            &value.to_string(),
        )?;
        Ok(())
    }
}

bitflags! {
    /// `LogfmtAutoFields` is used to determine what fields of a log::Record should be rendered into
    /// the final logfmt string by the `LogfmtFormatter`. The default set is LEVEL | MESSAGE | MODULE_PATH
    /// | EXTRA
    pub struct LogfmtAutoFields: u32 {
        /// Include a `level` field indicating the level the message was logged at.
        const LEVEL = 1;
        /// Include the `message` field containing the message passed to the log directive
        const MESSAGE = 1 << 1;
        /// Include a `target` field, corresponding to the target of the log directive
        const TARGET = 1 << 2;
        /// Include the `module` field set on the log record
        const MODULE_PATH = 1 << 3;
        /// Include the `file` field set on the log record
        const FILE = 1 << 4;
        /// Include the `line` field associated with the log directive.
        const LINE = 1 << 5;
        /// Include any extra fields specified via the structured logging API, if enabled.
        #[cfg(feature = "kv_unstable")]
        const EXTRA = 1 << 6;
    }
}

impl Default for LogfmtAutoFields {
    fn default() -> Self {
        #[cfg(feature = "kv_unstable")]
        {
            LogfmtAutoFields::LEVEL
                | LogfmtAutoFields::MESSAGE
                | LogfmtAutoFields::MODULE_PATH
                | LogfmtAutoFields::EXTRA
        }

        #[cfg(not(feature = "kv_unstable"))]
        {
            LogfmtAutoFields::LEVEL | LogfmtAutoFields::MESSAGE | LogfmtAutoFields::MODULE_PATH
        }
    }
}
