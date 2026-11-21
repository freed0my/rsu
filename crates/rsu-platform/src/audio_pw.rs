//! PipeWire playback stream.
//!
//! The caller's `fill` closure runs on PipeWire's real-time data thread once
//! per graph cycle and must not block or allocate. Each call also carries the
//! cycle's timing, which is what the game clock is built from.

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

#[derive(Clone, Copy, Debug)]
pub struct CycleTime {
    /// CLOCK_MONOTONIC time of the graph cycle.
    pub now_ns: u64,
    /// Frames until a sample written in this cycle is audible, or negative
    /// if PipeWire could not tell.
    pub delay_frames: f64,
}

pub struct PwOptions {
    pub sample_rate: u32,
    pub channels: u32,
    /// Requested graph quantum in frames. PipeWire treats it as a wish: the
    /// smallest request among active clients wins, within the server limits.
    pub quantum: u32,
}

pub struct PwOutput {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// Set once the stream reaches the streaming state.
    pub streaming: Arc<AtomicBool>,
}

impl Drop for PwOutput {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn start<F>(opts: PwOptions, fill: F) -> Result<PwOutput, String>
where
    F: FnMut(&mut [f32], CycleTime) + Send + 'static,
{
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let streaming = Arc::new(AtomicBool::new(false));
    let streaming2 = streaming.clone();
    let thread = std::thread::Builder::new()
        .name("rsu-pw".into())
        .spawn(move || {
            if let Err(e) = run(opts, fill, quit_rx, streaming2, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PwOutput { quit: quit_tx, thread: Some(thread), streaming }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("PipeWire thread exited during start-up".into()),
    }
}

fn run<F>(
    opts: PwOptions,
    fill: F,
    quit_rx: pw::channel::Receiver<()>,
    streaming: Arc<AtomicBool>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String>
where
    F: FnMut(&mut [f32], CycleTime) + Send + 'static,
{
    let e = |e: pw::Error| e.to_string();
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(e)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(e)?;
    let core = context.connect_rc(None).map_err(e)?;

    let latency = format!("{}/{}", opts.quantum, opts.sample_rate);
    let stream = pw::stream::StreamBox::new(
        &core,
        "rsu",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Game",
            *pw::keys::NODE_NAME => "rsu",
            *pw::keys::APP_NAME => "rsu",
            *pw::keys::NODE_LATENCY => latency.as_str(),
        },
    )
    .map_err(e)?;

    let channels = opts.channels as usize;
    let rate = opts.sample_rate as f64;
    let stride = std::mem::size_of::<f32>() * channels;
    let _listener = stream
        .add_local_listener_with_user_data(fill)
        .state_changed(move |_, _, _old, new| match new {
            pw::stream::StreamState::Streaming => streaming.store(true, Ordering::Relaxed),
            pw::stream::StreamState::Error(msg) => eprintln!("rsu: PipeWire stream error: {msg}"),
            _ => streaming.store(false, Ordering::Relaxed),
        })
        .process(move |stream, fill| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let requested = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let mut frames = 0;
            if let Some(bytes) = data.data() {
                // SAFETY: f32 has no invalid bit patterns; align_to_mut only
                // hands out the correctly aligned middle part.
                let (head, samples, _) = unsafe { bytes.align_to_mut::<f32>() };
                if head.is_empty() {
                    let capacity = samples.len() / channels;
                    // `requested` is how much this cycle needs. Writing more
                    // would queue audio and add exactly that much latency.
                    frames = if requested > 0 { requested.min(capacity) } else { capacity };
                    fill(&mut samples[..frames * channels], cycle_time(stream, rate));
                }
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
        .register()
        .map_err(e)?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(opts.sample_rate);
    info.set_channels(opts.channels);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let pod_bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|e| format!("{e:?}"))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&pod_bytes).ok_or("bad format pod")?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(e)?;

    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// Timing of the current cycle, straight from the graph driver.
fn cycle_time(stream: &pw::stream::Stream, rate: f64) -> CycleTime {
    // SAFETY: pw_time is plain data; the call is documented as safe from the
    // process callback and is told the size of the struct we pass.
    let mut t: pw::sys::pw_time = unsafe { std::mem::zeroed() };
    let ok =
        unsafe { pw::sys::pw_stream_get_time_n(stream.as_raw_ptr(), &mut t, std::mem::size_of::<pw::sys::pw_time>()) }
            == 0;
    if !ok || t.now <= 0 || t.rate.denom == 0 {
        return CycleTime { now_ns: rsu_rt::clock::now_ns(), delay_frames: -1.0 };
    }
    // `delay` is in units of `rate` (a fraction of a second); `buffered` is
    // frames still inside the stream's converter.
    let delay_s = t.delay as f64 * t.rate.num as f64 / t.rate.denom as f64;
    CycleTime { now_ns: t.now as u64, delay_frames: delay_s * rate + t.buffered as f64 }
}
