//! Tones made here, for the sounds Teams asks for by name when no file for them is on disk. They
//! are plain sine tones and sound nothing like Teams' own; a file in the notify dir replaces them.

use crate::notify::Pcm;

const RATE: u32 = 48_000;

/// The tone for a Teams sound file name, if there is one.
pub fn builtin(name: &str) -> Option<Pcm> {
    // Each part is (seconds, frequencies in Hz); no frequencies is a pause. A looped sound ends
    // with its pause, so the loop does not run the end into the start.
    let parts: &[(f32, &[f32])] = match name {
        // An incoming call: two bursts of the North American ring, then a pause.
        "ring.mp3" => &[
            (0.4, &[440.0, 480.0]),
            (0.2, &[]),
            (0.4, &[440.0, 480.0]),
            (2.0, &[]),
        ],
        // The caller's ringback.
        "Teams_Call_Ringing.mp3" => &[(1.0, &[425.0]), (3.0, &[])],
        // The call is over: three falling beeps.
        "Teams_Call_Ended.mp3" => &[
            (0.15, &[660.0]),
            (0.05, &[]),
            (0.15, &[520.0]),
            (0.05, &[]),
            (0.25, &[400.0]),
        ],
        _ => return None,
    };
    let mut samples = Vec::new();
    for (seconds, freqs) in parts {
        let n = (seconds * RATE as f32) as usize;
        let fade = (0.01 * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let sum: f32 = freqs
                .iter()
                .map(|f| (2.0 * std::f32::consts::PI * f * t).sin())
                .sum();
            // Ramps at both ends keep the bursts from clicking.
            let edge = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
            let level = if freqs.is_empty() {
                0.0
            } else {
                0.25 * edge * sum / freqs.len() as f32
            };
            samples.push((level * f32::from(i16::MAX)) as i16);
        }
    }
    Some(Pcm {
        rate: RATE,
        channels: 1,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_known_sounds_have_tones_and_others_do_not() {
        let ring = builtin("ring.mp3").unwrap();
        assert_eq!(ring.channels, 1);
        // 0.4 + 0.2 + 0.4 + 2.0 seconds.
        assert_eq!(ring.samples.len(), 3 * RATE as usize);
        assert!(ring.samples.iter().any(|&s| s.abs() > 1000));
        assert!(
            ring.samples[ring.samples.len() - 1000..]
                .iter()
                .all(|&s| s == 0)
        );
        assert!(builtin("Teams_Call_Ringing.mp3").is_some());
        assert!(builtin("Teams_Call_Ended.mp3").is_some());
        assert!(builtin("00_teams_basic_notification_r4_default.mp3").is_none());
    }

    #[test]
    fn tones_stay_well_below_full_scale() {
        for name in ["ring.mp3", "Teams_Call_Ringing.mp3", "Teams_Call_Ended.mp3"] {
            let peak = builtin(name).unwrap().samples.iter().map(|s| s.abs()).max();
            assert!(peak.unwrap() < i16::MAX / 3, "{name}");
        }
    }
}
