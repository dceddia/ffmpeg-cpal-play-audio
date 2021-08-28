extern crate ffmpeg_next as ffmpeg;

use cpal::{Sample, SampleFormat};
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::{Sample as FFmpegSample, input};
use ffmpeg::frame;
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling::{context::Context as ResamplingContext};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::RingBuffer;

trait SampleFormatConversion {
    fn as_ffmpeg_sample(&self) -> FFmpegSample;
}

impl SampleFormatConversion for SampleFormat {
    fn as_ffmpeg_sample(&self) -> FFmpegSample {
        match self {
            Self::I16 => FFmpegSample::I16(SampleType::Packed),
            Self::U16 => {
                panic!("ffmpeg resampler doesn't support u16")
            }, 
            Self::F32 => FFmpegSample::F32(SampleType::Packed)
        }
    }
}

fn write_audio<T: Sample>(data: &mut [T], samples: &mut ringbuf::Consumer<T>, _: &cpal::OutputCallbackInfo) {
    for d in data {
        // copy as many samples as we have.
        // if we run out, write silence
        match samples.pop() {
            Some(sample) => *d = sample,
            None => *d = Sample::from(&0.0)
        }
    }
}

fn init_cpal() -> (cpal::Device, cpal::SupportedStreamConfig) {
    let device = cpal::default_host()
        .default_output_device()
        .expect("no output device available");

    // Create an output stream for the audio so we can play it
    // NOTE: If system doesn't support the file's sample rate, the program will panic when we try to play,
    //       so we'll need to resample the audio to a supported config
    let supported_config_range = device.supported_output_configs()
        .expect("error querying audio output configs")
        .next()
        .expect("no supported audio config found");

    // Pick the best (highest) sample rate
    (device, supported_config_range.with_max_sample_rate())
}

// Interpret the audio frame's data as packed (alternating channels, 12121212, as opposed to planar 11112222)
pub fn packed<T: frame::audio::Sample>(frame: &frame::Audio) -> &[T] {
    if !frame.is_packed() {
        panic!("data is not packed");
    }

    if !<T as frame::audio::Sample>::is_valid(frame.format(), frame.channels()) {
        panic!("unsupported type");
    }

    unsafe { std::slice::from_raw_parts((*frame.as_ptr()).data[0] as *const T, frame.samples() * frame.channels() as usize) }
}

fn main() -> Result<(), ffmpeg::Error> {
    ffmpeg::init().unwrap();

    let file = &std::env::args().nth(1).expect("Cannot open file.");

    // Initialize cpal for playing audio
    let (device, stream_config) = init_cpal();

    // Open the file
    let mut ictx = input(&file)?;

    // Find the audio stream and its index
    let audio = ictx
        .streams()
        .best(MediaType::Audio)
        .ok_or(ffmpeg::Error::StreamNotFound)?;
    let audio_stream_index = audio.index();

    // Create a decoder
    let mut audio_decoder = audio.codec().decoder().audio()?;

    // Set up a resampler for the audio
    let mut resampler = ResamplingContext::get(
        audio_decoder.format(),
        audio_decoder.channel_layout(),
        audio_decoder.rate(),
        
        stream_config.sample_format().as_ffmpeg_sample(),
        audio_decoder.channel_layout(),
        stream_config.sample_rate().0
    )?;

    // A buffer to hold audio samples
    let buffer = RingBuffer::<f32>::new(8192);
    let (mut producer, mut consumer) = buffer.split();
    
    // Set up the audio output stream
    let audio_stream = match stream_config.sample_format() {
        SampleFormat::F32 => device.build_output_stream(&stream_config.into(), move |data: &mut [f32], cbinfo| {
            // Copy to the audio buffer (if there aren't enough samples, write_audio will write silence)
            write_audio(data, &mut consumer, &cbinfo)
        }, |err| {
            eprintln!("error occurred on the audio output stream: {}", err)
        }),
        SampleFormat::I16 => panic!("i16 output format unimplemented"),
        SampleFormat::U16 => panic!("u16 output format unimplemented")
    }.unwrap();

    let mut receive_and_queue_audio_frames =
        |decoder: &mut ffmpeg::decoder::Audio| -> Result<(), ffmpeg::Error> {
            let mut decoded = frame::Audio::empty();

            // Ask the decoder for frames
            while decoder.receive_frame(&mut decoded).is_ok() {
                // Resample the frame's audio into another frame
                let mut resampled = frame::Audio::empty();
                resampler.run(&decoded, &mut resampled)?;

                // DON'T just use resampled.data(0).len() -- it might not be fully populated
                // Grab the right number of bytes based on sample count, bytes per sample, and number of channels.
                let both_channels = packed(&resampled);

                // Sleep until the buffer has enough space for all of the samples
                // (the producer will happily accept a partial write, which we don't want)
                while producer.remaining() < both_channels.len() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }

                // Buffer the samples for playback
                producer.push_slice(both_channels);
            }
            Ok(())
        };

    // Start playing
    audio_stream.play().unwrap();

    // The main loop!
    for (stream, packet) in ictx.packets() {
        // Look for audio packets (ignore video and others)
        if stream.index() == audio_stream_index {
            // Send the packet to the decoder; it will combine them into frames.
            // In practice though, 1 packet = 1 frame
            audio_decoder.send_packet(&packet)?;

            // Queue the audio for playback (and block if the queue is full)
            receive_and_queue_audio_frames(&mut audio_decoder)?;
        }
    }

    Ok(())
}