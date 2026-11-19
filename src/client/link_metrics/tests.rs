use super::*;

fn sample(elapsed_ms: u64, uploaded: u64, downloaded: u64) -> Sample {
    Sample {
        elapsed_ms,
        uploaded,
        downloaded,
    }
}

#[test]
fn link_metrics_measure_monotonic_deltas_and_smooth_only_valid_samples() {
    let mut sampler = Sampler::default();
    assert_eq!(sampler.observe(sample(10, 10, 30)), Status::Sampling);
    assert_eq!(
        sampler.observe(sample(1010, 1510, 3030)),
        Status::Ready {
            uploaded: 1500,
            downloaded: 3000
        }
    );
    assert_eq!(
        sampler.observe(sample(2010, 1510, 3030)),
        Status::Ready {
            uploaded: 750,
            downloaded: 1500
        }
    );
    let mut idle = Sampler::default();
    assert_eq!(idle.observe(sample(1, 0, 0)), Status::Sampling);
    assert_eq!(
        idle.observe(sample(1001, 0, 0)),
        Status::Ready {
            uploaded: 0,
            downloaded: 0
        }
    );
}

#[test]
fn link_metrics_reset_on_clock_counter_generation_gaps_and_overflow() {
    for bad in [
        sample(1000, 500, 500),
        sample(999, 500, 500),
        sample(12000, 500, 500),
        sample(2000, 99, 500),
        sample(2000, 500, 99),
        sample(1500, u64::MAX, u64::MAX),
    ] {
        let mut sampler = Sampler::default();
        sampler.observe(sample(1000, 100, 100));
        assert_eq!(sampler.observe(bad), Status::Sampling, "{bad:?}");
    }
    let mut recorder = Recorder::disabled();
    recorder.uploaded = u64::MAX;
    recorder.downloaded = 500;
    recorder.sent(1);
    assert_eq!(recorder.totals(), (1, 0));
    recorder.downloaded = u64::MAX;
    recorder.received(2);
    assert_eq!(recorder.totals(), (0, 2));
}

#[test]
fn link_metrics_wire_sample_is_fixed_bounded_and_rejects_prefixes() {
    let value = sample(u64::MAX, 123, 456);
    let encoded = value.encode();
    assert_eq!(Sample::decode(&encoded), Some(value));
    assert_eq!(Sample::decode(&encoded[..31]), None);
    let mut longer = encoded.to_vec();
    longer.push(0);
    assert_eq!(Sample::decode(&longer), None);
    let mut other = encoded;
    other[0] ^= 1;
    assert_eq!(Sample::decode(&other), None);
}

#[test]
fn link_metrics_view_keeps_connection_identity_and_rejects_old_updates() {
    let mut view = View::default();
    view.begin(1, "abc01234ffff", Status::Sampling);
    assert_eq!(view.rows()[0], "App data #abc01234");
    assert!(view.rows()[1].contains("Sampling"));
    view.update(
        1,
        Status::Ready {
            uploaded: 1500,
            downloaded: 2500000,
        },
    );
    assert_eq!(view.rows()[1], "↑ 1.5 KB/s");
    assert_eq!(view.rows()[2], "↓ 2.5 MB/s");
    view.begin(2, "def56789ffff", Status::Sampling);
    assert!(!view.update(1, Status::Disconnected));
    view.begin(1, "old-connection", Status::Disconnected);
    assert_eq!(view.rows()[0], "App data #def56789");
    assert_eq!(view.status, Status::Sampling);
    assert!(view.update(2, Status::Disconnected));
    assert!(!view.rows().join(" ").contains("0 B/s"));
    view.update(2, Status::Unavailable);
    assert!(view.rows()[1].contains("unavailable"));
    assert!(!view.rows().join(" ").contains("0 B/s"));
    assert_eq!(rate(1), "1 B/s");
    assert_eq!(rate(u64::MAX), "18.4 EB/s");
}
