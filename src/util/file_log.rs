// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::{self, Timespec, Tm};

const ONE_DAY_SECONDS: u64 = 60 * 60 * 24;

fn systemtime_to_tm(t: SystemTime) -> Tm {
    let duration = t.duration_since(UNIX_EPOCH).unwrap();
    let spec = Timespec::new(duration.as_secs() as i64, duration.subsec_nanos() as i32);
    time::at(spec)
}

fn compute_rollover_time(tm: Tm) -> Tm {
    let day_start_tm = Tm {
        tm_hour: 0,
        tm_min: 0,
        tm_sec: 0,
        tm_nsec: 0,
        ..tm
    };
    let duration = time::Duration::from_std(Duration::new(ONE_DAY_SECONDS, 0)).unwrap();
    (day_start_tm.to_utc() + duration).to_local()
}

/// Returns a Tm at the time one day before the given Tm.
/// It expects the argument `tm` to be in local timezone. The resulting Tm is in local timezone.
fn one_day_before(tm: Tm) -> Tm {
    let duration = time::Duration::from_std(Duration::new(ONE_DAY_SECONDS, 0)).unwrap();
    (tm.to_utc() - duration).to_local()
}

fn open_log_file(path: &str) -> io::Result<File> {
    let p = Path::new(path);
    let parent = p.parent().unwrap();
    if !parent.is_dir() {
        fs::create_dir_all(parent)?
    }
    OpenOptions::new().append(true).create(true).open(path)
}

pub struct RotatingFileLogger {
    rollover_time: Tm,
    file_path: String,
    file: File,
}

impl RotatingFileLogger {
    pub fn new(path: &str) -> io::Result<Self> {
        let file = open_log_file(path)?;
        let file_attr = fs::metadata(path).unwrap();
        let file_modified_time = file_attr.modified().unwrap();
        let rollover_time = compute_rollover_time(systemtime_to_tm(file_modified_time));
        let ret = Self {
            rollover_time,
            file_path: path.to_string(),
            file,
        };
        Ok(ret)
    }

    fn open(&mut self) -> io::Result<()> {
        self.file = open_log_file(&self.file_path)?;
        Ok(())
    }

    fn should_rollover(&mut self) -> bool {
        time::now() > self.rollover_time
    }

    fn do_rollover(&mut self) -> io::Result<()> {
        self.close()?;
        let mut s = self.file_path.clone();
        s.push_str(".");
        s.push_str(&time::strftime("%Y%m%d", &one_day_before(self.rollover_time)).unwrap());
        fs::rename(&self.file_path, &s)?;
        self.update_rollover_time();
        self.open()
    }

    fn update_rollover_time(&mut self) {
        let now = time::now();
        self.rollover_time = compute_rollover_time(now);
    }

    fn close(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Write for RotatingFileLogger {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.should_rollover() {
            self.do_rollover()?;
        };
        self.file.flush()
    }
}

impl Drop for RotatingFileLogger {
    fn drop(&mut self) {
        self.close().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::prelude::*;
    use std::path::Path;
    use time::{self, Timespec};

    use tempdir::TempDir;
    use utime;

    use super::{RotatingFileLogger, ONE_DAY_SECONDS};

    #[test]
    fn test_one_day_before() {
        let tm = time::strptime("2016-08-30", "%Y-%m-%d").unwrap().to_local();
        let one_day_ago = time::strptime("2016-08-29", "%Y-%m-%d").unwrap().to_local();
        assert_eq!(one_day_ago, super::one_day_before(tm));
    }

    fn file_exists(file: &str) -> bool {
        let path = Path::new(file);
        path.exists() && path.is_file()
    }

    #[test]
    fn test_rotating_file_logger() {
        let tmp_dir = TempDir::new("").unwrap();
        let log_file = tmp_dir
            .path()
            .join("test_rotating_file_logger.log")
            .to_str()
            .unwrap()
            .to_string();
        // create a file with mtime == one day ago
        {
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&log_file)
                .unwrap();
            file.write_all(b"hello world!").unwrap();
        }
        let ts = time::now().to_timespec();
        let one_day_ago = Timespec::new(ts.sec - ONE_DAY_SECONDS as i64, ts.nsec);
        let time_in_sec = one_day_ago.sec as u64;
        utime::set_file_times(&log_file, time_in_sec, time_in_sec).unwrap();
        // initialize the logger
        let mut core = RotatingFileLogger::new(&log_file).unwrap();
        assert!(core.should_rollover());
        core.do_rollover().unwrap();
        // check the rotated file exist
        let mut rotated_file = log_file.clone();
        rotated_file.push_str(".");
        let file_suffix_time =
            super::one_day_before(super::compute_rollover_time(time::at(one_day_ago)));
        rotated_file.push_str(&time::strftime("%Y%m%d", &file_suffix_time).unwrap());
        assert!(file_exists(&rotated_file));
        assert!(!core.should_rollover());
    }
}
