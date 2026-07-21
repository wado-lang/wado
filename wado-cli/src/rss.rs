struct RssSample {
    current: u64,
    peak: u64,
}

impl RssSample {
    fn read() -> Option<Self> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        Self::parse(&status)
    }

    fn parse(status: &str) -> Option<Self> {
        let current = parse_kib_field(status, "VmRSS:")?;
        let peak = parse_kib_field(status, "VmHWM:").unwrap_or(current);
        Some(Self { current, peak })
    }

    fn current_peak_mib(&self) -> String {
        format!("{}/{} MiB", to_mib(self.current), to_mib(self.peak))
    }
}

fn parse_kib_field(status: &str, key: &str) -> Option<u64> {
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix(key))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib * 1024)
}

fn to_mib(bytes: u64) -> u64 {
    (bytes + 512 * 1024) / (1024 * 1024)
}

pub(crate) fn live_suffix() -> Option<String> {
    RssSample::read().map(|s| format!(" · rss {}", s.current_peak_mib()))
}

pub(crate) fn summary_line() -> Option<String> {
    RssSample::read().map(|s| format!("rss:     peak {} MiB", to_mib(s.peak)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_STATUS: &str = "\
Name:\twado
VmPeak:\t 2097152 kB
VmSize:\t 1048576 kB
VmHWM:\t   716800 kB
VmRSS:\t   524288 kB
Threads:\t8
";

    #[test]
    fn parses_current_and_peak_rss() {
        let sample = RssSample::parse(SAMPLE_STATUS).expect("both fields present");
        assert_eq!(sample.current, 524288 * 1024);
        assert_eq!(sample.peak, 716800 * 1024);
        assert_eq!(sample.current_peak_mib(), "512/700 MiB");
    }

    #[test]
    fn falls_back_to_current_when_peak_field_absent() {
        let sample = RssSample::parse("VmRSS:\t 100 kB\n").expect("current present");
        assert_eq!(sample.current, 100 * 1024);
        assert_eq!(sample.peak, 100 * 1024);
    }

    #[test]
    fn returns_none_without_a_current_field() {
        assert!(RssSample::parse("VmHWM:\t 100 kB\n").is_none());
        assert!(RssSample::parse("").is_none());
    }

    #[test]
    fn field_key_match_is_exact_including_colon() {
        let status = "VmRSSFoo:\t 999 kB\nVmRSS:\t 42 kB\nVmHWM:\t 84 kB\n";
        let sample = RssSample::parse(status).expect("exact field present");
        assert_eq!(sample.current, 42 * 1024);
    }

    #[test]
    fn rounds_mib_to_nearest_rather_than_truncating_down() {
        let just_under_512_mib = 512 * 1024 * 1024 - 100 * 1024;
        assert_eq!(to_mib(just_under_512_mib), 512);
    }
}
