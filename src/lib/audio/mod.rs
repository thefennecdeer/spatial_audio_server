use metres::Metres;
use nannou;
use nannou::audio::Buffer;
use nannou::math::{MetricSpace, Point2};
use std;
use std::collections::HashMap;
pub use self::sound::Sound;
pub use self::source::Source;
pub use self::speaker::Speaker;
pub use self::wav::Wav;

pub mod sound;
pub mod source;
pub mod speaker;
pub mod wav;

/// Sounds should only be output to speakers that are nearest to avoid the need to render each
/// sound to every speaker on the map.
pub const PROXIMITY_LIMIT: Metres = Metres(5.0);
/// The proximity squared (for more efficient distance comparisons).
pub const PROXIMITY_LIMIT_2: Metres = Metres(PROXIMITY_LIMIT.0 * PROXIMITY_LIMIT.0);

/// The maximum number of audio channels.
pub const MAX_CHANNELS: usize = 32;

/// The desired sample rate of the output stream.
pub const SAMPLE_RATE: f64 = 44_100.0;

/// The desired number of frames requested at a time.
pub const FRAMES_PER_BUFFER: usize = 64;

/// Simplified type alias for the nannou audio output stream used by the audio server.
pub type OutputStream = nannou::audio::stream::Output<Model>;

/// State that lives on the audio thread.
pub struct Model {
    /// A map from audio sound IDs to the audio sounds themselves.
    pub sounds: HashMap<sound::Id, Sound>,
    /// A map from speaker IDs to the speakers themselves.
    pub speakers: HashMap<speaker::Id, Speaker>,
    /// A buffer for collecting the speakers within proximity of the sound's position.
    speakers_in_proximity: Vec<(Amplitude, usize)>,
    /// A buffer for collecting the speakers within proximity of the sound's position.
    unmixed_samples: Vec<f32>,
}

impl Model {
    /// Initialise the `Model`.
    pub fn new() -> Self {
        // A map from audio sound IDs to the audio sounds themselves.
        let sounds: HashMap<sound::Id, Sound> = HashMap::with_capacity(1024);

        // A map from speaker IDs to the speakers themselves.
        let speakers: HashMap<speaker::Id, Speaker> = HashMap::with_capacity(MAX_CHANNELS);

        // A buffer for collecting the speakers within proximity of the sound's position.
        let speakers_in_proximity = Vec::with_capacity(MAX_CHANNELS);

        // A buffer for collecting frames from `Sound`s that have not yet been mixed and written.
        let unmixed_samples = vec![0.0; 1024];

        Model {
            sounds,
            speakers,
            speakers_in_proximity,
            unmixed_samples,
        }
    }
}

/// The function given to nannou to use for rendering.
pub fn render(mut model: Model, mut buffer: Buffer) -> (Model, Buffer) {
    {
        let Model {
            ref mut sounds,
            ref mut speakers_in_proximity,
            ref mut unmixed_samples,
            ref speakers,
        } = model;

        // For each sound, request `buffer.len()` number of frames and sum them onto the
        // relevant output channels.
        for (&_sound_id, sound) in sounds {
            let num_samples = buffer.len_frames() * sound.channels;

            // Clear the unmixed samples, ready to collect the new ones.
            unmixed_samples.clear();
            {
                let signal = (0..num_samples).map(|_| sound.signal.next()[0]);
                unmixed_samples.extend(signal);
            }

            // Mix the audio from the signal onto each of the output channels.
            for i in 0..sound.channels {

                // Find the absolute position of the channel.
                let channel_point =
                    channel_point(sound.point, i, sound.channels, sound.spread, sound.radians);

                // Find the speakers that are closest to the channel.
                find_closest_speakers(&channel_point, speakers_in_proximity, &speakers);
                let mut sample_index = i;
                for frame in buffer.frames_mut() {
                    let channel_sample = unmixed_samples[sample_index];
                    for &(amp, channel) in speakers_in_proximity.iter() {
                        // Only write to the channels that will be read by the audio device.
                        if let Some(sample) = frame.get_mut(channel) {
                            *sample += channel_sample * amp;
                        }
                    }
                    sample_index += sound.channels;
                }
            }
        }
    }

    (model, buffer)
}

pub fn channel_point(
    sound_point: Point2<Metres>,
    channel_index: usize,
    total_channels: usize,
    spread: Metres,
    radians: f32,
) -> Point2<Metres>
{
    assert!(channel_index < total_channels);
    if total_channels == 1 {
        sound_point
    } else {
        let phase = channel_index as f32 / total_channels as f32;
        let default_radians = phase * std::f32::consts::PI * 2.0;
        let radians = (radians + default_radians) as f64;
        let rel_x = Metres(-radians.cos() * spread.0);
        let rel_y = Metres(radians.sin() * spread.0);
        let x = sound_point.x + rel_x;
        let y = sound_point.y + rel_y;
        Point2 { x, y }
    }
}

type Amplitude = f32;

// Converts the given squared distance to an amplitude multiplier.
//
// The squared distance is used to avoid the need to perform square root.
fn distance_2_to_amplitude(Metres(distance_2): Metres) -> Amplitude {
    // TODO: This is a linear tail off - experiment with exponential tail off.
    1.0 - (distance_2 / PROXIMITY_LIMIT_2.0) as f32
}

fn find_closest_speakers(
    point: &Point2<Metres>,
    closest: &mut Vec<(Amplitude, usize)>, // Amplitude along with the speaker's channel index.
    speakers: &HashMap<speaker::Id, Speaker>,
) {
    closest.clear();
    let point_f = Point2 { x: point.x.0, y: point.y.0 };
    for (_, speaker) in speakers.iter() {
        let speaker_point_f = Point2 { x: speaker.point.x.0, y: speaker.point.y.0 };
        let distance_2 = Metres(point_f.distance2(speaker_point_f));
        if distance_2 < PROXIMITY_LIMIT_2 {
            // Use a function to map distance to amp.
            let amp = distance_2_to_amplitude(distance_2);
            closest.push((amp, speaker.channel));
        }
    }
}
