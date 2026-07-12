use fold::pipeline::{Aggregate, Filter, KeyBy, terminal};
use fold::stream::Stream;
use serde::{Deserialize, Serialize};

const HOUR_MS: u64 = 60 * 60 * 1000;
const DAY_MS: u64 = 24 * HOUR_MS;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Reading {
    sensor: String,
    at_ms: u64,
    temp_c_tenths: i32,
    rain_mm_tenths: i32,
    wind_mph_tenths: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HourStats {
    samples: i64,
    temp_c_tenths_sum: i64,
    rain_mm_tenths_sum: i64,
    wind_mph_tenths_sum: i64,
}

impl HourStats {
    fn avg_temp_c(&self) -> f64 {
        tenths_avg(self.temp_c_tenths_sum, self.samples)
    }

    fn avg_wind_mph(&self) -> f64 {
        tenths_avg(self.wind_mph_tenths_sum, self.samples)
    }

    fn rain_mm(&self) -> f64 {
        self.rain_mm_tenths_sum as f64 / 10.0
    }
}

macro_rules! print_snapshot {
    ($st:expr) => {
        $st.rtx(|(total, hourly, daily_rain, raw)| {
            println!("total readings: {}", total.get());

            let mut hours: Vec<_> = hourly.iter().collect();
            hours.sort_by_key(|(hour, _)| *hour);
            println!("hourly weather:");
            for (hour, stats) in hours {
                println!(
                    "  hour {hour}: {} samples, avg temp {:.1} c, avg wind {:.1} mph, rain {:.1} mm",
                    stats.samples,
                    stats.avg_temp_c(),
                    stats.avg_wind_mph(),
                    stats.rain_mm()
                );
            }

            let mut days: Vec<_> = daily_rain.iter().collect();
            days.sort_by_key(|(day, _)| *day);
            println!("daily rain:");
            for (day, rain_tenths) in days {
                println!("  day {day}: {:.1} mm", rain_tenths as f64 / 10.0);
            }

            println!("raw readings still queryable: {}", raw.iter().count());
        });
    };
}

fn main() {
    let db_path = std::env::temp_dir().join("bog-kit-timeseries.db");
    let _ = std::fs::remove_dir_all(&db_path);

    let mut st = Stream::new(
        &db_path,
        (
            terminal::Count::new("readings_total"),
            // Aggregate emits a changelog per key (-old, +new); Table is
            // the natural sink for it — it always holds the current
            // accumulator per key, point-readable and iterable
            KeyBy::new(
                |r: &Reading| r.at_ms / HOUR_MS,
                Aggregate::new(
                    "weather_by_hour",
                    hourly_step,
                    terminal::Table::new("hourly_weather"),
                ),
            ),
            Filter::new(
                |r: &Reading| r.rain_mm_tenths > 0,
                KeyBy::new(
                    |r: &Reading| r.at_ms / DAY_MS,
                    Aggregate::new("rain_by_day", rain_step, terminal::Table::new("daily_rain")),
                ),
            ),
            terminal::Bag::<Reading>::new("raw_readings"),
        ),
    );

    let readings = sample_readings();
    st.wtx(|tx| {
        for reading in &readings {
            tx.insert(reading);
        }
    });

    println!("after initial ingest");
    print_snapshot!(st);

    // fold updates every materialized view by retraction, not by rebuilding.
    st.wtx(|tx| tx.remove(&readings[2]));

    println!("\nafter retracting one rainy reading");
    print_snapshot!(st);
}

fn hourly_step(acc: &mut HourStats, reading: &Reading, delta: isize) {
    let delta = delta as i64;
    acc.samples += delta;
    acc.temp_c_tenths_sum += reading.temp_c_tenths as i64 * delta;
    acc.rain_mm_tenths_sum += reading.rain_mm_tenths as i64 * delta;
    acc.wind_mph_tenths_sum += reading.wind_mph_tenths as i64 * delta;
}

fn rain_step(acc: &mut i64, reading: &Reading, delta: isize) {
    *acc += reading.rain_mm_tenths as i64 * delta as i64;
}

fn sample_readings() -> Vec<Reading> {
    vec![
        Reading {
            sensor: "roof".to_string(),
            at_ms: 0,
            temp_c_tenths: 184,
            rain_mm_tenths: 0,
            wind_mph_tenths: 52,
        },
        Reading {
            sensor: "roof".to_string(),
            at_ms: 15 * 60 * 1000,
            temp_c_tenths: 189,
            rain_mm_tenths: 0,
            wind_mph_tenths: 68,
        },
        Reading {
            sensor: "roof".to_string(),
            at_ms: 45 * 60 * 1000,
            temp_c_tenths: 181,
            rain_mm_tenths: 12,
            wind_mph_tenths: 91,
        },
        Reading {
            sensor: "field".to_string(),
            at_ms: HOUR_MS + 5 * 60 * 1000,
            temp_c_tenths: 176,
            rain_mm_tenths: 4,
            wind_mph_tenths: 114,
        },
        Reading {
            sensor: "field".to_string(),
            at_ms: HOUR_MS + 40 * 60 * 1000,
            temp_c_tenths: 172,
            rain_mm_tenths: 0,
            wind_mph_tenths: 102,
        },
        Reading {
            sensor: "roof".to_string(),
            at_ms: DAY_MS + 10 * 60 * 1000,
            temp_c_tenths: 201,
            rain_mm_tenths: 7,
            wind_mph_tenths: 41,
        },
    ]
}

fn tenths_avg(sum: i64, count: i64) -> f64 {
    if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64 / 10.0
    }
}
