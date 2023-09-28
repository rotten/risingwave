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

use chrono::{Datelike, NaiveTime, Timelike};
use risingwave_common::types::{Date, Decimal, Interval, Time, Timestamp, Timestamptz, F64};
use risingwave_expr::{function, ExprError, Result};

use crate::scalar::timestamptz::time_zone_err;

fn extract_date(date: impl Datelike, unit: &str) -> Option<Decimal> {
    Some(if unit.eq_ignore_ascii_case("millennium") {
        ((date.year() - 1) / 1000 + 1).into()
    } else if unit.eq_ignore_ascii_case("century") {
        ((date.year() - 1) / 100 + 1).into()
    } else if unit.eq_ignore_ascii_case("decade") {
        (date.year() / 10).into()
    } else if unit.eq_ignore_ascii_case("year") {
        date.year().into()
    } else if unit.eq_ignore_ascii_case("isoyear") {
        date.iso_week().year().into()
    } else if unit.eq_ignore_ascii_case("quarter") {
        ((date.month() - 1) / 3 + 1).into()
    } else if unit.eq_ignore_ascii_case("month") {
        date.month().into()
    } else if unit.eq_ignore_ascii_case("week") {
        date.iso_week().week().into()
    } else if unit.eq_ignore_ascii_case("day") {
        date.day().into()
    } else if unit.eq_ignore_ascii_case("doy") {
        date.ordinal().into()
    } else if unit.eq_ignore_ascii_case("dow") {
        date.weekday().num_days_from_sunday().into()
    } else if unit.eq_ignore_ascii_case("isodow") {
        date.weekday().number_from_monday().into()
    } else {
        return None;
    })
}

fn extract_time(time: impl Timelike, unit: &str) -> Option<Decimal> {
    let usecs = || time.second() as u64 * 1_000_000 + (time.nanosecond() / 1000) as u64;
    Some(if unit.eq_ignore_ascii_case("hour") {
        time.hour().into()
    } else if unit.eq_ignore_ascii_case("minute") {
        time.minute().into()
    } else if unit.eq_ignore_ascii_case("second") {
        Decimal::from_i128_with_scale(usecs() as i128, 6)
    } else if unit.eq_ignore_ascii_case("millisecond") {
        Decimal::from_i128_with_scale(usecs() as i128, 3)
    } else if unit.eq_ignore_ascii_case("microsecond") {
        usecs().into()
    } else if unit.eq_ignore_ascii_case("epoch") {
        let usecs =
            time.num_seconds_from_midnight() as u64 * 1_000_000 + (time.nanosecond() / 1000) as u64;
        Decimal::from_i128_with_scale(usecs as i128, 6)
    } else {
        return None;
    })
}

#[function("extract(varchar, date) -> decimal")]
pub fn extract_from_date(unit: &str, date: Date) -> Result<Decimal> {
    if unit.eq_ignore_ascii_case("epoch") {
        let epoch = date.0.and_time(NaiveTime::default()).timestamp();
        return Ok(epoch.into());
    } else if unit.eq_ignore_ascii_case("julian") {
        const UNIX_EPOCH_DAY: i32 = 719_163;
        let julian = date.0.num_days_from_ce() - UNIX_EPOCH_DAY + 2_440_588;
        return Ok(julian.into());
    };
    extract_date(date.0, unit).ok_or_else(|| invalid_unit("date unit", unit))
}

#[function("extract(varchar, time) -> decimal")]
pub fn extract_from_time(unit: &str, time: Time) -> Result<Decimal> {
    extract_time(time.0, unit).ok_or_else(|| invalid_unit("time unit", unit))
}

#[function("extract(varchar, timestamp) -> decimal")]
pub fn extract_from_timestamp(unit: &str, timestamp: Timestamp) -> Result<Decimal> {
    if unit.eq_ignore_ascii_case("epoch") {
        let epoch = Decimal::from_i128_with_scale(timestamp.0.timestamp_micros() as i128, 6);
        return Ok(epoch);
    } else if unit.eq_ignore_ascii_case("julian") {
        let epoch = Decimal::from_i128_with_scale(timestamp.0.timestamp_micros() as i128, 6);
        return Ok(epoch / (24 * 60 * 60).into() + 2_440_588.into());
    };
    extract_date(timestamp.0, unit)
        .or_else(|| extract_time(timestamp.0, unit))
        .ok_or_else(|| invalid_unit("timestamp unit", unit))
}

#[function("extract(varchar, timestamptz) -> decimal")]
pub fn extract_from_timestamptz(unit: &str, tz: Timestamptz) -> Result<Decimal> {
    if unit.eq_ignore_ascii_case("epoch") {
        Ok(Decimal::from_i128_with_scale(tz.timestamp_micros() as _, 6))
    } else {
        // TODO(#5826): all other units depend on implicit session TimeZone
        Err(invalid_unit("timestamp with time zone units", unit))
    }
}

#[function("extract(varchar, timestamptz, varchar) -> decimal")]
pub fn extract_from_timestamptz_at_timezone(
    unit: &str,
    input: Timestamptz,
    timezone: &str,
) -> Result<Decimal> {
    use chrono::Offset as _;

    let time_zone = Timestamptz::lookup_time_zone(timezone).map_err(time_zone_err)?;
    let instant_local = input.to_datetime_in_zone(time_zone);

    if unit.eq_ignore_ascii_case("epoch") {
        Ok(Decimal::from_i128_with_scale(
            input.timestamp_micros() as _,
            6,
        ))
    } else if unit.eq_ignore_ascii_case("timezone") {
        let east_secs = instant_local.offset().fix().local_minus_utc();
        Ok(east_secs.into())
    } else if unit.eq_ignore_ascii_case("timezone_hour") {
        let east_secs = instant_local.offset().fix().local_minus_utc();
        Ok((east_secs / 3600).into())
    } else if unit.eq_ignore_ascii_case("timezone_minute") {
        let east_secs = instant_local.offset().fix().local_minus_utc();
        Ok((east_secs % 3600 / 60).into())
    } else {
        let timestamp = instant_local.naive_local();
        extract_from_timestamp(unit, timestamp.into())
    }
}

#[function("extract(varchar, interval) -> decimal")]
pub fn extract_from_interval(unit: &str, interval: Interval) -> Result<Decimal> {
    Ok(if unit.eq_ignore_ascii_case("millennium") {
        (interval.years_field() / 1000).into()
    } else if unit.eq_ignore_ascii_case("century") {
        (interval.years_field() / 100).into()
    } else if unit.eq_ignore_ascii_case("decade") {
        (interval.years_field() / 10).into()
    } else if unit.eq_ignore_ascii_case("year") {
        interval.years_field().into()
    } else if unit.eq_ignore_ascii_case("quarter") {
        (interval.months_field() / 3 + 1).into()
    } else if unit.eq_ignore_ascii_case("month") {
        interval.months_field().into()
    } else if unit.eq_ignore_ascii_case("day") {
        interval.days_field().into()
    } else if unit.eq_ignore_ascii_case("hour") {
        interval.hours_field().into()
    } else if unit.eq_ignore_ascii_case("minute") {
        interval.minutes_field().into()
    } else if unit.eq_ignore_ascii_case("second") {
        Decimal::from_i128_with_scale(interval.seconds_in_micros() as i128, 6)
    } else if unit.eq_ignore_ascii_case("millisecond") {
        Decimal::from_i128_with_scale(interval.seconds_in_micros() as i128, 3)
    } else if unit.eq_ignore_ascii_case("microsecond") {
        interval.seconds_in_micros().into()
    } else if unit.eq_ignore_ascii_case("epoch") {
        Decimal::from_i128_with_scale(interval.epoch_in_micros(), 6)
    } else {
        return Err(invalid_unit("interval unit", unit));
    })
}

#[function("date_part(varchar, date) -> float8")]
pub fn date_part_from_date(unit: &str, date: Date) -> Result<F64> {
    // date_part of date manually cast to timestamp
    // https://github.com/postgres/postgres/blob/REL_15_2/src/backend/catalog/system_functions.sql#L123
    extract_from_timestamp(unit, date.into())?
        .try_into()
        .map_err(|_| ExprError::NumericOutOfRange)
}

#[function("date_part(varchar, time) -> float8")]
pub fn date_part_from_time(unit: &str, time: Time) -> Result<F64> {
    extract_from_time(unit, time)?
        .try_into()
        .map_err(|_| ExprError::NumericOutOfRange)
}

#[function("date_part(varchar, timestamptz) -> float8")]
pub fn date_part_from_timestamptz(unit: &str, input: Timestamptz) -> Result<F64> {
    extract_from_timestamptz(unit, input)?
        .try_into()
        .map_err(|_| ExprError::NumericOutOfRange)
}

#[function("date_part(varchar, timestamptz, varchar) -> float8")]
pub fn date_part_from_timestamptz_at_timezone(
    unit: &str,
    input: Timestamptz,
    timezone: &str,
) -> Result<F64> {
    extract_from_timestamptz_at_timezone(unit, input, timezone)?
        .try_into()
        .map_err(|_| ExprError::NumericOutOfRange)
}

#[function("date_part(varchar, timestamp) -> float8")]
pub fn date_part_from_timestamp(unit: &str, timestamp: Timestamp) -> Result<F64> {
    extract_from_timestamp(unit, timestamp)?
        .try_into()
        .map_err(|_| ExprError::NumericOutOfRange)
}

#[function("date_part(varchar, interval) -> float8")]
pub fn date_part_from_interval(unit: &str, interval: Interval) -> Result<F64> {
    extract_from_interval(unit, interval)?
        .try_into()
        .map_err(|_| ExprError::NumericOutOfRange)
}

fn invalid_unit(name: &'static str, unit: &str) -> ExprError {
    ExprError::InvalidParam {
        name,
        reason: format!("\"{unit}\" not recognized or supported").into(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime};

    use super::*;

    #[test]
    fn test_date() {
        let date = Date::new(NaiveDate::parse_from_str("2021-11-22", "%Y-%m-%d").unwrap());
        assert_eq!(extract_from_date("DAY", date).unwrap(), 22.into());
        assert_eq!(extract_from_date("MONTH", date).unwrap(), 11.into());
        assert_eq!(extract_from_date("YEAR", date).unwrap(), 2021.into());
        assert_eq!(extract_from_date("DOW", date).unwrap(), 1.into());
        assert_eq!(extract_from_date("DOY", date).unwrap(), 326.into());
        assert_eq!(extract_from_date("MILLENNIUM", date).unwrap(), 3.into());
        assert_eq!(extract_from_date("CENTURY", date).unwrap(), 21.into());
        assert_eq!(extract_from_date("DECADE", date).unwrap(), 202.into());
        assert_eq!(extract_from_date("ISOYEAR", date).unwrap(), 2021.into());
        assert_eq!(extract_from_date("QUARTER", date).unwrap(), 4.into());
        assert_eq!(extract_from_date("WEEK", date).unwrap(), 47.into());
        assert_eq!(extract_from_date("ISODOW", date).unwrap(), 1.into());
        assert_eq!(extract_from_date("EPOCH", date).unwrap(), 1637539200.into());
        assert_eq!(extract_from_date("JULIAN", date).unwrap(), 2_459_541.into());
    }

    #[test]
    fn test_timestamp() {
        let ts = Timestamp::new(
            NaiveDateTime::parse_from_str("2021-11-22 12:4:2.575400", "%Y-%m-%d %H:%M:%S%.f")
                .unwrap(),
        );
        let extract = |f, i| extract_from_timestamp(f, i).unwrap().to_string();
        assert_eq!(extract("MILLENNIUM", ts), "3");
        assert_eq!(extract("CENTURY", ts), "21");
        assert_eq!(extract("DECADE", ts), "202");
        assert_eq!(extract("ISOYEAR", ts), "2021");
        assert_eq!(extract("YEAR", ts), "2021");
        assert_eq!(extract("QUARTER", ts), "4");
        assert_eq!(extract("MONTH", ts), "11");
        assert_eq!(extract("WEEK", ts), "47");
        assert_eq!(extract("DAY", ts), "22");
        assert_eq!(extract("DOW", ts), "1");
        assert_eq!(extract("ISODOW", ts), "1");
        assert_eq!(extract("DOY", ts), "326");
        assert_eq!(extract("HOUR", ts), "12");
        assert_eq!(extract("MINUTE", ts), "4");
        assert_eq!(extract("SECOND", ts), "2.575400");
        assert_eq!(extract("MILLISECOND", ts), "2575.400");
        assert_eq!(extract("MICROSECOND", ts), "2575400");
        assert_eq!(extract("EPOCH", ts), "1637582642.575400");
        assert_eq!(extract("JULIAN", ts), "2459541.5028075856481481481481");
    }

    #[test]
    fn test_extract_from_time() {
        let time: Time = "23:22:57.123450".parse().unwrap();
        let extract = |f, i| extract_from_time(f, i).unwrap().to_string();
        assert_eq!(extract("Hour", time), "23");
        assert_eq!(extract("Minute", time), "22");
        assert_eq!(extract("Second", time), "57.123450");
        assert_eq!(extract("Millisecond", time), "57123.450");
        assert_eq!(extract("Microsecond", time), "57123450");
        assert_eq!(extract("Epoch", time), "84177.123450");
    }

    #[test]
    fn test_extract_from_interval() {
        let interval: Interval = "2345 years 1 mon 250 days 23:22:57.123450".parse().unwrap();
        let extract = |f, i| extract_from_interval(f, i).unwrap().to_string();
        assert_eq!(extract("Millennium", interval), "2");
        assert_eq!(extract("Century", interval), "23");
        assert_eq!(extract("Decade", interval), "234");
        assert_eq!(extract("Year", interval), "2345");
        assert_eq!(extract("Month", interval), "1");
        assert_eq!(extract("Day", interval), "250");
        assert_eq!(extract("Hour", interval), "23");
        assert_eq!(extract("Minute", interval), "22");
        assert_eq!(extract("Second", interval), "57.123450");
        assert_eq!(extract("Millisecond", interval), "57123.450");
        assert_eq!(extract("Microsecond", interval), "57123450");
        assert_eq!(extract("Epoch", interval), "74026848177.123450");
        assert!(extract_from_interval("Nanosecond", interval).is_err());
        assert!(extract_from_interval("Week", interval).is_err());

        let interval: Interval = "-2345 years -1 mon -250 days -23:22:57.123450"
            .parse()
            .unwrap();
        assert_eq!(extract("Millennium", interval), "-2");
        assert_eq!(extract("Century", interval), "-23");
        assert_eq!(extract("Decade", interval), "-234");
        assert_eq!(extract("Year", interval), "-2345");
        assert_eq!(extract("Month", interval), "-1");
        assert_eq!(extract("Day", interval), "-250");
        assert_eq!(extract("Hour", interval), "-23");
        assert_eq!(extract("Minute", interval), "-22");
        assert_eq!(extract("Second", interval), "-57.123450");
        assert_eq!(extract("Millisecond", interval), "-57123.450");
        assert_eq!(extract("Microsecond", interval), "-57123450");
        assert_eq!(extract("Epoch", interval), "-74026848177.123450");
        assert!(extract_from_interval("Nanosecond", interval).is_err());
        assert!(extract_from_interval("Week", interval).is_err());
    }
}
