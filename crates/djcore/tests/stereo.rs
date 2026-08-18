//! Stereo decode + mono downmix integration tests.

#[test]
fn stereo_wav_decodes_two_interleaved_channels() {
    // Given an asymmetric stereo WAV (left-only tone).
    let path = std::env::temp_dir().join("automixah_stereo_test.wav");
    {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec).expect("create");
        for i in 0..44_100 {
            #[expect(clippy::cast_precision_loss, reason = "test index")]
            let l = (i as f32 / 44_100.0 * 2.0 * std::f32::consts::PI * 440.0).sin() * 0.5;
            w.write_sample(l).expect("l");
            w.write_sample(0.0_f32).expect("r");
        }
    }

    // When decoding.
    let bytes = std::fs::read(&path).expect("read");
    let decoded = djcore::decoder::DecoderRegistry::with_symphonia()
        .decode(&bytes, "wav")
        .expect("decode");

    // Then two channels are present, interleaved, right is silent.
    assert_eq!(decoded.channels, 2);
    assert_eq!(decoded.samples.len(), 2 * 44_100);
    assert!(decoded.samples.chunks(2).all(|f| f[1].abs() < 1e-6));
    assert!(decoded.samples.chunks(2).any(|f| f[0].abs() > 0.1));

    // And the mono downmix halves the amplitude of the left tone.
    let mono = decoded.to_mono();
    assert_eq!(mono.len(), 44_100);
    assert!(mono.iter().all(|s| s.abs() < 0.26));
}
