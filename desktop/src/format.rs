//! Human-readable formatting for counters and sizes.

pub fn human_count(value: u64) -> String {
    const UNITS: [&str; 5] = ["", "K", "M", "B", "T"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1_000.0 && unit < UNITS.len() - 1 {
        scaled /= 1_000.0;
        unit += 1;
    }

    if unit == 0 {
        value.to_string()
    } else {
        format!("{scaled:.1}{}", UNITS[unit])
    }
}

pub fn human_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1_024.0 && unit < UNITS.len() - 1 {
        scaled /= 1_024.0;
        unit += 1;
    }
    format!("{scaled:.1} {}", UNITS[unit])
}

pub fn format_duration(milliseconds: f64) -> String {
    let total_seconds = (milliseconds.max(0.0) / 1_000.0) as u64;
    let hours = total_seconds / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_bytes_scale_into_readable_units() {
        assert_eq!(human_count(950), "950");
        assert_eq!(human_count(1_500), "1.5K");
        assert_eq!(human_bytes(2_048), "2.0 KiB");
        assert_eq!(format_duration(65_000.0), "01:05");
        assert_eq!(format_duration(3_700_000.0), "01:01:40");
    }
}
