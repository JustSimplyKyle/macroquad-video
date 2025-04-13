#![feature(iter_map_windows)]

use std::{
    iter,
    thread::sleep,
    time::{Duration, Instant},
};

use eyre::ContextCompat;
use ffmpeg_the_third::{
    self as ffmpeg, Packet, Stream, codec,
    filter::Graph,
    format::{self, Pixel, context::Input},
    frame::{Audio, Video},
    media,
    software::scaling::Flags,
    threading,
};
use macroquad::{
    audio::{Sound, load_sound_from_bytes, play_sound_once},
    prelude::*,
};

fn retain_aspect_ratio_scale(frame: &Video) -> Result<Texture2D, eyre::Error> {
    let src_width = frame.width();
    let src_height = frame.height();
    let mut dst_width = screen_width() as u32;
    // stride fixes
    dst_width -= dst_width % 32;
    let dst_height = screen_height() as u32;

    let mut graph = Graph::new();

    graph.parse(&format!(
            "buffer=video_size={src_width}x{src_height}:pix_fmt=rgba:time_base=1/1:sar=1/1,
                scale=force_original_aspect_ratio=decrease:w={dst_width}:h={dst_height}:flags=bilinear,
                pad=w={dst_width}:h={dst_height}:x=(ow-iw)/2:y=(oh-ih)/2,
                buffersink"
        ))?;

    graph.validate()?;

    graph
        .get("Parsed_buffer_0")
        .context("buffer does not exists")?
        .source()
        .add(frame)?;

    let mut output = Video::empty();

    graph
        .get("Parsed_buffersink_3")
        .context("buffer sink does not exists")?
        .sink()
        .frame(&mut output)?;

    let texture = Texture2D::from_rgba8(
        output.width().try_into()?,
        output.height().try_into()?,
        output.data(0),
    );

    eyre::Result::Ok(texture)
}

fn decode_frame<'a, T: Iterator<Item = (Stream<'a>, Packet)>>(
    video_packets: Vec<(Stream<'a>, Packet)>,
    audio_packets: T,
) -> eyre::Result<(
    impl Iterator<Item = Texture2D> + use<'a, T>,
    impl Iterator<Item = Audio> + use<'a, T>,
    f64,
)> {
    let (avg_frame_rate, vstream) = video_packets
        .first()
        .map(|x| (x.0.avg_frame_rate().into(), x.0.parameters()))
        .context("not possible")?;

    let mut audio_packets = audio_packets.peekable();
    let astream = audio_packets
        .peek()
        .map(|x| x.0.parameters())
        .context("not possible")?;

    let mut vcodec = codec::context::Context::from_parameters(vstream)?;
    let acodec = codec::context::Context::from_parameters(astream)?;
    if let Ok(paralleism) = std::thread::available_parallelism() {
        vcodec.set_threading(threading::Config {
            kind: threading::Type::Frame,
            count: paralleism.get(),
        });
    }

    let mut vdecoder = vcodec.decoder().video()?;
    let mut adecoder = acodec.decoder().audio()?;

    let mut scaler = ffmpeg::software::scaling::Context::get(
        vdecoder.format(),
        vdecoder.width(),
        vdecoder.height(),
        Pixel::RGBA,
        vdecoder.width(),
        vdecoder.height(),
        Flags::BILINEAR,
    )?;

    let audio = audio_packets
        .map(|x| x.1)
        .chain(std::iter::once(Packet::empty()))
        .filter_map(move |packet| {
            unsafe {
                if packet.is_empty() {
                    adecoder.send_eof().ok()?;
                } else {
                    adecoder.send_packet(&packet).ok()?;
                }
            }
            let mut decoded_audio = Audio::empty();
            let mut audio = Vec::new();
            while adecoder.receive_frame(&mut decoded_audio).is_ok() {
                let mut resampler = decoded_audio
                    .resampler2(
                        format::Sample::I16(format::sample::Type::Packed),
                        decoded_audio.ch_layout(),
                        decoded_audio.rate(),
                    )
                    .ok()?;
                let mut wav = Audio::empty();
                resampler.run(&decoded_audio, &mut wav).ok()?;
                audio.push(wav);
            }
            Some(audio)
        })
        .flatten();

    let video = video_packets
        .into_iter()
        .map(|x| x.1)
        .chain(std::iter::once(Packet::empty()))
        .filter_map(move |packet| {
            unsafe {
                if packet.is_empty() {
                    vdecoder.send_eof().ok()?;
                } else {
                    vdecoder.send_packet(&packet).ok()?;
                }
            }
            let mut decoded_video = Video::empty();
            let mut video = Vec::new();
            while vdecoder.receive_frame(&mut decoded_video).is_ok() {
                let mut rgb_frame = Video::empty();
                scaler.run(&decoded_video, &mut rgb_frame).ok()?;
                video.push(rgb_frame);
            }
            Some(video)
        })
        .flatten()
        .map(|frame| retain_aspect_ratio_scale(&frame))
        .map_while(Result::<_, eyre::Error>::ok);

    Ok((video, audio, avg_frame_rate))
}

struct VideoPlayer<Iter: iter::Iterator<Item = Texture2D>> {
    frames: iter::Peekable<Iter>,
    frames_played: usize,
    frame_rate: f64,
    audio: Sound,
    instant: Instant,
    broken: Duration,
}

impl<Iter: Iterator<Item = Texture2D>> VideoPlayer<Iter> {
    fn frame_limiter(&mut self) {
        let frame_duration = Duration::from_secs_f64(1. / self.frame_rate);
        let elapsed = self.instant.elapsed();

        if elapsed < frame_duration {
            if frame_duration - elapsed >= self.broken {
                sleep(frame_duration - elapsed - self.broken);
                self.broken = Duration::ZERO;
            } else {
                sleep(Duration::ZERO);
                self.broken = self.broken.saturating_sub(frame_duration - elapsed);
            }
        } else {
            if self.broken > Duration::from_millis(1000) {
                error!(
                    "compensation frames exceed 1000ms in total, please make sure settings are correct!"
                );
            }
            self.broken += elapsed - frame_duration;
            warn!(
                "took tooooo long to render!\nwill try to compensate it by early playing the few next frames by {:?}",
                self.broken
            );
        }
        self.instant = Instant::now();
    }

    fn draw_video_by_frame(&mut self) {
        clear_background(BLACK);

        if self.frames_played == 0 {
            play_sound_once(&self.audio);
        }

        let Some(texture) = &self.frames.next() else {
            return;
        };

        draw_texture(texture, 0., 0., WHITE);
        let text_color = WHITE;

        draw_text(
            &format!("{:.2}", 1. / get_frame_time()),
            90.,
            90.,
            70.,
            text_color,
        );

        self.frame_limiter();

        self.frames_played += 1;
    }
}

async fn get_video_player(
    input: &mut Input,
) -> eyre::Result<VideoPlayer<impl Iterator<Item = Texture2D>>> {
    let vstream_id = input
        .streams()
        .best(media::Type::Video)
        .context("stream not found")?
        .index();

    let astream_id = input
        .streams()
        .best(media::Type::Audio)
        .context("stream not found")?
        .index();

    let packets = input.packets().filter_map(Result::ok);

    let (video_packets, not_video_packets): (Vec<_>, Vec<_>) =
        packets.partition(|x| x.0.index() == vstream_id);

    let audio_packets = not_video_packets
        .into_iter()
        .filter(move |x| x.0.index() == astream_id);

    let (frames, audio, frame_rate) = decode_frame(video_packets, audio_packets)?;

    let audio = audio.peekable();

    let buffer = build_wav_from_raw(audio)?;

    let sound = load_sound_from_bytes(&buffer).await?;

    let video_player = VideoPlayer {
        frames: frames.peekable(),
        frames_played: 0,
        frame_rate,
        audio: sound,
        instant: Instant::now(),
        broken: Duration::ZERO,
    };

    Ok(video_player)
}

fn build_wav_from_raw(
    mut audio: iter::Peekable<impl Iterator<Item = Audio>>,
) -> Result<Vec<u8>, eyre::Error> {
    let mut buffer = Vec::new();
    let cursor = std::io::Cursor::new(&mut buffer);

    let first = audio.peek().context("empty audio stream")?;

    let channels = first.ch_layout().channels();

    let mut writer = hound::WavWriter::new(
        cursor,
        hound::WavSpec {
            channels: channels.try_into()?,
            sample_rate: first.rate(),
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )?;

    for audio in audio {
        let data = audio.data(0);
        let sample_size = 2;
        let frame_size = sample_size * channels;

        for frame in data.chunks_exact(frame_size.try_into()?) {
            for ch in 0..channels {
                let i = (ch * sample_size) as usize;
                let sample = i16::from_le_bytes([frame[i], frame[i + 1]]);
                writer.write_sample(sample)?;
            }
        }
    }
    writer.finalize()?;
    Ok(buffer)
}

#[macroquad::main("MyGame")]
async fn main() -> eyre::Result<()> {
    ffmpeg::init()?;
    rand::srand(miniquad::date::now().to_bits());

    let mut input = ffmpeg::format::input("prodigy.webm")?;
    let mut video_player = get_video_player(&mut input).await?;

    loop {
        video_player.draw_video_by_frame();
        next_frame().await;
    }
}
