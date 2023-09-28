// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use risingwave_common::types::{Interval, Timestamp, Timestamptz};
use risingwave_expr::expr::BoxedExpression;
use risingwave_expr::{build_function, function, ExprError, Result};

use super::timestamptz::{timestamp_at_time_zone, timestamptz_at_time_zone};

#[function("date_trunc(varchar, timestamp) -> timestamp")]
pub fn date_trunc_timestamp(field: &str, ts: Timestamp) -> Result<Timestamp> {
    Ok(match field.to_ascii_lowercase().as_str() {
        "microseconds" => ts.truncate_micros(),
        "milliseconds" => ts.truncate_millis(),
        "second" => ts.truncate_second(),
        "minute" => ts.truncate_minute(),
        "hour" => ts.truncate_hour(),
        "day" => ts.truncate_day(),
        "week" => ts.truncate_week(),
        "month" => ts.truncate_month(),
        "quarter" => ts.truncate_quarter(),
        "year" => ts.truncate_year(),
        "decade" => ts.truncate_decade(),
        "century" => ts.truncate_century(),
        "millennium" => ts.truncate_millennium(),
        _ => return Err(invalid_field_error(field)),
    })
}

// Only to register this signature to function signature map.
#[build_function("date_trunc(varchar, timestamptz) -> timestamptz")]
fn build_date_trunc_timestamptz_implicit_zone(
    _return_type: risingwave_common::types::DataType,
    _children: Vec<BoxedExpression>,
) -> Result<BoxedExpression> {
    Err(ExprError::UnsupportedFunction(
        "date_trunc of timestamptz should have been rewritten to include timezone".into(),
    ))
}

#[function("date_trunc(varchar, timestamptz, varchar) -> timestamptz")]
pub fn date_trunc_timestamptz_at_timezone(
    field: &str,
    ts: Timestamptz,
    timezone: &str,
) -> Result<Timestamptz> {
    let timestamp = timestamptz_at_time_zone(ts, timezone)?;
    let truncated = date_trunc_timestamp(field, timestamp)?;
    timestamp_at_time_zone(truncated, timezone)
}

#[function("date_trunc(varchar, interval) -> interval")]
pub fn date_trunc_interval(field: &str, interval: Interval) -> Result<Interval> {
    Ok(match field.to_ascii_lowercase().as_str() {
        "microseconds" => interval,
        "milliseconds" => interval.truncate_millis(),
        "second" => interval.truncate_second(),
        "minute" => interval.truncate_minute(),
        "hour" => interval.truncate_hour(),
        "day" => interval.truncate_day(),
        "week" => return Err(ExprError::UnsupportedFunction(
            "interval units \"week\" not supported because months usually have fractional weeks"
                .into(),
        )),
        "month" => interval.truncate_month(),
        "quarter" => interval.truncate_quarter(),
        "year" => interval.truncate_year(),
        "decade" => interval.truncate_decade(),
        "century" => interval.truncate_century(),
        "millennium" => interval.truncate_millennium(),
        _ => return Err(invalid_field_error(field)),
    })
}

#[inline]
fn invalid_field_error(field: &str) -> ExprError {
    ExprError::InvalidParam {
        name: "field",
        reason: format!("invalid field {field:?}. must be one of: microseconds, milliseconds, second, minute, hour, day, week, month, quarter, year, decade, century, millennium").into(),
    }
}
