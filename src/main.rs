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
    frame::{Audio, Video, audio::Sample},
    media,
    software::{resampling, scaling::Flags},
    threading,
};
use macroquad::{
    audio::{Sound, load_sound_from_bytes, play_sound, play_sound_once},
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

fn decode_frame<'a>(
    vpackets: Vec<&mut (Stream<'a>, Packet)>,
    apackets: Vec<&mut (Stream<'a>, Packet)>,
) -> eyre::Result<(
    impl Iterator<Item = Texture2D>,
    impl Iterator<Item = Audio>,
    f64,
)> {
    let (avg_frame_rate, vstream) = vpackets
        .first()
        .map(|x| (x.0.avg_frame_rate().into(), x.0.parameters()))
        .context("not possible")?;

    let astream = apackets
        .first()
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

    let boxed_empty = Box::new(Packet::empty());

    let audio = apackets
        .into_iter()
        .map(|x| &x.1)
        .chain(std::iter::once(&*Box::leak(boxed_empty.clone())))
        .filter_map(move |packet| {
            unsafe {
                if packet.is_empty() {
                    adecoder.send_eof().ok()?;
                } else {
                    adecoder.send_packet(packet).ok()?;
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

    let video = vpackets
        .into_iter()
        .map(|x| &x.1)
        .chain(std::iter::once(&*Box::leak(boxed_empty)))
        .filter_map(move |packet| {
            unsafe {
                if packet.is_empty() {
                    vdecoder.send_eof().ok()?;
                } else {
                    vdecoder.send_packet(packet).ok()?;
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

async fn draw_video(
    frames: impl Iterator<Item = (Texture2D)>,
    audio: impl Iterator<Item = (Audio)>,
    frame_rate: f64,
) -> eyre::Result<()> {
    let frame_duration = 1.0 / frame_rate;
    let mut audio = audio.peekable();

    let frame_duration = Duration::from_secs_f64(frame_duration);

    let mut buffer = Vec::new();
    let cursor = std::io::Cursor::new(&mut buffer);

    let mut writer = hound::WavWriter::new(
        cursor,
        hound::WavSpec {
            channels: audio
                .peek()
                .context("empty audio stream")?
                .ch_layout()
                .channels()
                .try_into()?,
            sample_rate: audio.peek().context("empty audio stream")?.rate(),
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )?;
    for audio in audio {
        let channels: u16 = audio.ch_layout().channels().try_into()?;
        let data = audio.data(0);
        let sample_size = 2;
        let frame_size = sample_size * channels;

        for frame in data.chunks_exact(frame_size.into()) {
            for ch in 0..channels {
                let i = (ch * sample_size) as usize;
                let sample = i16::from_le_bytes([frame[i], frame[i + 1]]);
                writer.write_sample(sample)?;
            }
        }
    }

    writer.finalize()?;

    let sound = load_sound_from_bytes(&buffer).await?;

    play_sound_once(&sound);

    let mut broken = Duration::new(0, 0);

    let mut instant = Instant::now();

    for texture in frames {
        clear_background(BLACK);
        draw_texture(&texture, 0., 0., WHITE);
        draw_text(
            &format!("{:.2}", 1. / get_frame_time()),
            90.,
            90.,
            70.,
            BLACK,
        );

        let elapsed = instant.elapsed();

        if elapsed < frame_duration {
            if frame_duration - elapsed >= broken {
                sleep(frame_duration - elapsed - broken);
                broken = Duration::new(0, 0);
            } else {
                sleep(Duration::new(0, 0));
                broken = broken.saturating_sub(frame_duration - elapsed);
            }
        } else {
            broken += elapsed - frame_duration;
            warn!(
                "took tooooo long to render!\nwill try to compensate it by early playing the next frame by {:?}",
                broken
            );
        }
        instant = Instant::now();
        next_frame().await;
    }
    Ok(())
}

#[macroquad::main("MyGame")]
async fn main() -> eyre::Result<()> {
    ffmpeg::init()?;

    request_new_screen_size(800., 450.);
    let mut input = ffmpeg::format::input("song.webm")?;
    let vstream = input
        .streams()
        .best(media::Type::Video)
        .context("stream not found")?
        .index();
    let astream = input
        .streams()
        .best(media::Type::Audio)
        .context("stream not found")?
        .index();
    let mut packets = input.packets().filter_map(Result::ok).collect::<Vec<_>>();
    let (vpackets, mut not_v_packets): (Vec<_>, Vec<_>) =
        packets.iter_mut().partition(|x| x.0.index() == vstream);

    let apackets = not_v_packets
        .iter_mut()
        .filter(|x| x.0.index() == astream)
        .map(|x| &mut **x)
        .collect::<Vec<_>>();

    let (frames, audio, frame_rate) = decode_frame(vpackets, apackets)?;

    rand::srand(miniquad::date::now().to_bits());

    draw_video(frames, audio, frame_rate).await?;

    loop {
        clear_background(RED);
        next_frame().await;
    }
}
