// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::convert::TryInto;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use libc::O_NONBLOCK;

use rate_limiter::{RateLimiter, TokenBucket};

/// Wrapper for configuring the microVM boot source.
pub mod boot_source;
/// Wrapper for configuring the block devices.
pub mod drive;
/// Wrapper over the microVM general information attached to the microVM.
pub mod instance_info;
/// Wrapper for configuring the logger.
pub mod logger;
/// Wrapper for configuring the memory and CPU of the microVM.
pub mod machine_config;
/// Wrapper for configuring the metrics.
pub mod metrics;
/// Wrapper for configuring the network devices attached to the microVM.
pub mod net;
/// Wrapper for configuring the vsock devices attached to the microVM.
pub mod vsock;

// TODO: Migrate the VMM public-facing code (i.e. interface) to use stateless structures,
// for receiving data/args, such as the below `RateLimiterConfig` and `TokenBucketConfig`.
// Also todo: find a better suffix than `Config`; it should illustrate the static nature
// of the enclosed data.
// Currently, data is passed around using live/stateful objects. Switching to static/stateless
// objects will simplify both the ownership model and serialization.
// Public access would then be more tightly regulated via `VmmAction`s, consisting of tuples like
// (entry-point-into-VMM-logic, stateless-args-structure).

/// A public-facing, stateless structure, holding all the data we need to create a TokenBucket
/// (live) object.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
pub struct TokenBucketConfig {
    /// See TokenBucket::size.
    pub size: u64,
    /// See TokenBucket::one_time_burst.
    pub one_time_burst: Option<u64>,
    /// See TokenBucket::refill_time.
    pub refill_time: u64,
}

impl Into<TokenBucket> for TokenBucketConfig {
    fn into(self) -> TokenBucket {
        TokenBucket::new(self.size, self.one_time_burst, self.refill_time)
    }
}

/// A public-facing, stateless structure, holding all the data we need to create a RateLimiter
/// (live) object.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RateLimiterConfig {
    /// Data used to initialize the RateLimiter::bandwidth bucket.
    pub bandwidth: Option<TokenBucketConfig>,
    /// Data used to initialize the RateLimiter::ops bucket.
    pub ops: Option<TokenBucketConfig>,
}

impl RateLimiterConfig {
    /// Updates the configuration, merging in new options from `new_config`.
    pub fn update(&mut self, new_config: &RateLimiterConfig) {
        if new_config.bandwidth.is_some() {
            self.bandwidth = new_config.bandwidth;
        }
        if new_config.ops.is_some() {
            self.ops = new_config.ops;
        }
    }
}

impl TryInto<RateLimiter> for RateLimiterConfig {
    type Error = io::Error;

    fn try_into(self) -> std::result::Result<RateLimiter, Self::Error> {
        let bw = self.bandwidth.unwrap_or_default();
        let ops = self.ops.unwrap_or_default();
        RateLimiter::new(
            bw.size,
            bw.one_time_burst,
            bw.refill_time,
            ops.size,
            ops.one_time_burst,
            ops.refill_time,
        )
    }
}

type Result<T> = std::result::Result<T, std::io::Error>;

/// Structure `Writer` used for writing to a FIFO.
pub struct Writer {
    line_writer: Mutex<io::LineWriter<File>>,
}

impl Writer {
    /// Create and open a FIFO for writing to it.
    /// In order to not block the instance if nobody is consuming the message that is flushed to the
    /// two pipes, we are opening it with `O_NONBLOCK` flag. In this case, writing to a pipe will
    /// start failing when reaching 64K of unconsumed content. Simultaneously,
    /// the `missed_metrics_count` metric will get increased.
    pub fn new(fifo_path: PathBuf) -> Result<Writer> {
        OpenOptions::new()
            .custom_flags(O_NONBLOCK)
            .read(true)
            .write(true)
            .open(&fifo_path)
            .map(|t| Writer {
                line_writer: Mutex::new(io::LineWriter::new(t)),
            })
    }

    fn get_line_writer(&self) -> MutexGuard<io::LineWriter<File>> {
        match self.line_writer.lock() {
            Ok(guard) => guard,
            // If a thread panics while holding this lock, the writer within should still be usable.
            // (we might get an incomplete log line or something like that).
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl io::Write for Writer {
    fn write(&mut self, msg: &[u8]) -> Result<(usize)> {
        let mut line_writer = self.get_line_writer();
        line_writer.write_all(msg).map(|()| msg.len())
    }

    fn flush(&mut self) -> Result<()> {
        let mut line_writer = self.get_line_writer();
        line_writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use utils::tempfile::TempFile;

    use super::*;

    #[test]
    fn test_rate_limiter_configs() {
        const SIZE: u64 = 1024 * 1024;
        const ONE_TIME_BURST: u64 = 1024;
        const REFILL_TIME: u64 = 1000;

        let b: TokenBucket = TokenBucketConfig {
            size: SIZE,
            one_time_burst: Some(ONE_TIME_BURST),
            refill_time: REFILL_TIME,
        }
        .into();
        assert_eq!(b.capacity(), SIZE);
        assert_eq!(b.one_time_burst(), ONE_TIME_BURST);
        assert_eq!(b.refill_time_ms(), REFILL_TIME);

        let mut rlconf = RateLimiterConfig {
            bandwidth: Some(TokenBucketConfig {
                size: SIZE,
                one_time_burst: Some(ONE_TIME_BURST),
                refill_time: REFILL_TIME,
            }),
            ops: Some(TokenBucketConfig {
                size: SIZE * 2,
                one_time_burst: None,
                refill_time: REFILL_TIME * 2,
            }),
        };
        let rl: RateLimiter = rlconf.try_into().unwrap();
        assert_eq!(rl.bandwidth().unwrap().capacity(), SIZE);
        assert_eq!(rl.bandwidth().unwrap().one_time_burst(), ONE_TIME_BURST);
        assert_eq!(rl.bandwidth().unwrap().refill_time_ms(), REFILL_TIME);
        assert_eq!(rl.ops().unwrap().capacity(), SIZE * 2);
        assert_eq!(rl.ops().unwrap().one_time_burst(), 0);
        assert_eq!(rl.ops().unwrap().refill_time_ms(), REFILL_TIME * 2);

        rlconf.update(&RateLimiterConfig {
            bandwidth: Some(TokenBucketConfig {
                size: SIZE * 2,
                one_time_burst: Some(ONE_TIME_BURST * 2),
                refill_time: REFILL_TIME * 2,
            }),
            ops: None,
        });
        assert_eq!(rlconf.bandwidth.unwrap().size, SIZE * 2);
        assert_eq!(
            rlconf.bandwidth.unwrap().one_time_burst,
            Some(ONE_TIME_BURST * 2)
        );
        assert_eq!(rlconf.bandwidth.unwrap().refill_time, REFILL_TIME * 2);
        assert_eq!(rlconf.ops.unwrap().size, SIZE * 2);
        assert_eq!(rlconf.ops.unwrap().one_time_burst, None);
        assert_eq!(rlconf.ops.unwrap().refill_time, REFILL_TIME * 2);
    }

    #[test]
    fn test_log_writer() {
        let log_file_temp =
            TempFile::new().expect("Failed to create temporary output logging file.");
        let good_file = log_file_temp.as_path().to_path_buf();
        let res = Writer::new(good_file);
        assert!(res.is_ok());

        let mut fw = res.unwrap();
        let msg = String::from("some message");
        assert!(fw.write(&msg.as_bytes()).is_ok());
        assert!(fw.flush().is_ok());
    }
}
